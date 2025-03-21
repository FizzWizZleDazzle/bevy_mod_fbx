use std::path::Path;

use anyhow::{anyhow, bail, Context};

use bevy::{
    asset::{
        io::Reader,
        AssetLoader, LoadContext, RenderAssetUsages,
    },
    math::{DVec2, DVec3, Vec2, Vec3},
    prelude::{
        BuildChildren, ChildBuild, debug, error, info, trace,
        FromWorld, Handle, Image, Mesh, Mesh3d, Name,
        MeshMaterial3d, Scene, StandardMaterial, Transform, 
        Visibility, World, WorldChildBuilder,
    },
    render::{
        mesh::{Indices, PrimitiveTopology, VertexAttributeValues},
        render_resource::AddressMode,
        renderer::RenderDevice,
    },
    image::{
        CompressedImageFormats, ImageSampler, ImageType, 
        ImageSamplerDescriptor,
    },
    utils::{ConditionalSendFuture, HashMap},
};

use fbxcel_dom::{
    any::AnyDocument,
    v7400::{
        data::{
            mesh::layer::TypedLayerElementHandle,
            texture::WrapMode,
        },
        object::{
            self,
            model::{ModelHandle, TypedModelHandle},
            texture::TextureHandle,
            ObjectId, TypedObjectHandle,
        },
        Document,
    },
};

#[cfg(feature = "profile")]
use bevy::log::info_span;

use crate::{
    data::{FbxMesh, FbxObject, FbxScene},
    error::FbxLoadingError,
    fbx_transform::FbxTransform,
    utils::{
        fbx_extend::{GlobalSettingsExt, ModelTreeRootExt},
        triangulate,
    },
    MaterialLoader,
};

/// Bevy is kinda "meters" based while FBX (or rather: stuff exported by maya) is in "centimeters"
/// Although it doesn't mean much in practice.
const FBX_TO_BEVY_SCALE_FACTOR: f32 = 0.01;

pub struct Loader<'b, 'w> {
    scene: FbxScene,
    load_context: &'b mut LoadContext<'w>,
    suported_compressed_formats: CompressedImageFormats,
    material_loaders: Vec<MaterialLoader>,
}

pub struct FbxLoader {
    supported: CompressedImageFormats,
    material_loaders: Vec<MaterialLoader>,
}
impl FromWorld for FbxLoader {
    fn from_world(world: &mut World) -> Self {
        let supported = match world.get_resource::<RenderDevice>() {
            Some(render_device) => CompressedImageFormats::from_features(render_device.features()),

            None => CompressedImageFormats::all(),
        };
        let loaders: crate::FbxMaterialLoaders = world.get_resource().cloned().unwrap_or_default();
        Self {
            supported,
            material_loaders: loaders.0,
        }
    }
}

impl AssetLoader for FbxLoader {
    type Asset = FbxScene;
    type Settings = ();
    type Error = FbxLoadingError;

    fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        load_context: &mut LoadContext,
    ) -> impl ConditionalSendFuture<Output = Result<Self::Asset, Self::Error>> {
        Box::pin(async move {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            let cursor = std::io::Cursor::new(bytes.as_slice());
            let reader = std::io::BufReader::new(cursor);
            let maybe_doc =
                AnyDocument::from_seekable_reader(reader).expect("Failed to load document");
            if let AnyDocument::V7400(_ver, doc) = maybe_doc {
                let loader =
                    Loader::new(self.supported, self.material_loaders.clone(), load_context);
                match loader.load(*doc).await {
                    Ok(scene) => Ok(scene),
                    Err(err) => {
                        error!("{err:?}");
                        Err(FbxLoadingError::Other(err.to_string()))
                    }
                }
            } else {
                Err(FbxLoadingError::IncorrectFileVersion)
            }
        })
    }
    fn extensions(&self) -> &[&str] {
        &["fbx"]
    }
}

fn spawn_scene(
    fbx_file_scale: f32,
    roots: &[ObjectId],
    hierarchy: &HashMap<ObjectId, FbxObject>,
    models: &HashMap<ObjectId, FbxMesh>,
) -> Scene {
    #[cfg(feature = "profile")]
    let _generate_scene_span = info_span!("generate_scene").entered();

    let mut scene_world = World::default();
    scene_world
        .spawn((
           Visibility::default(),
           Transform::from_scale(Vec3::ONE * FBX_TO_BEVY_SCALE_FACTOR * fbx_file_scale),
           Name::from("FbxScene"),
        ))
        .with_children(|commands| {
            for root in roots {
                spawn_scene_rec(*root, commands, hierarchy, models);
            }
        });
    Scene::new(scene_world)
}
fn spawn_scene_rec(
    current: ObjectId,
    commands: &mut WorldChildBuilder,
    hierarchy: &HashMap<ObjectId, FbxObject>,
    models: &HashMap<ObjectId, FbxMesh>,

) {
    let current_node = match hierarchy.get(&current) {
        Some(node) => node,
        None => return,
    };
    let mut entity = commands.spawn((
        Visibility::default(),
        current_node.transform,
    ));
    if let Some(name) = &current_node.name {
        entity.insert(Name::new(name.clone()));
    }
    entity.with_children(|commands| {
        if let Some(mesh) = models.get(&current) {
            for (mat, bevy_mesh) in mesh.materials.iter().zip(&mesh.bevy_mesh_handles) {
                let mut entity = commands.spawn((MeshMaterial3d(mat.clone()), Mesh3d(bevy_mesh.clone())));
                if let Some(name) = mesh.name.as_ref() {
                    entity.insert(Name::new(name.clone()));
                }
            }
        }
        for node_id in &current_node.children {
            spawn_scene_rec(*node_id, commands, hierarchy, models);
        }
    });
}

impl<'b, 'w> Loader<'b, 'w> {
    fn new(
        formats: CompressedImageFormats,
        loaders: Vec<MaterialLoader>,
        load_context: &'b mut LoadContext<'w>,
    ) -> Self {
        Self {
            scene: FbxScene::default(),
            load_context,
            material_loaders: loaders,
            suported_compressed_formats: formats,
        }
    }

    async fn load(mut self, doc: Document) -> anyhow::Result<FbxScene> {
        info!(
            "Started loading scene {}#FbxScene",
            self.load_context.path().to_string_lossy(),
        );
        let mut meshes = HashMap::new();
        let mut hierarchy = HashMap::new();

        let fbx_scale = doc
            .global_settings()
            .and_then(|g| g.fbx_scale())
            .unwrap_or(1.0);
        let roots = doc.model_roots();
        for root in &roots {
            traverse_hierarchy(*root, &mut hierarchy);
        }

        for obj in doc.objects() {
            if let TypedObjectHandle::Model(TypedModelHandle::Mesh(mesh)) = obj.get_typed() {
                meshes.insert(obj.object_id(), self.load_mesh(mesh).await?);
            }
        }
        let roots: Vec<_> = roots.into_iter().map(|obj| obj.object_id()).collect();
        let scene = spawn_scene(fbx_scale as f32, &roots, &hierarchy, &meshes);

        let load_context = &mut self.load_context;
        load_context.add_labeled_asset("Scene".to_string(), scene);

        let mut scene = self.scene;
        scene.hierarchy = hierarchy.clone();
        scene.roots = roots;
        load_context.add_labeled_asset("FbxScene".to_string(), scene.clone());
        info!(
            "Successfully loaded scene {}#FbxScene",
            load_context.path().to_string_lossy(),
        );
        Ok(scene)
    }

    fn load_bevy_mesh(
        &mut self,
        mesh_obj: object::geometry::MeshHandle,
        num_materials: usize,
    ) -> anyhow::Result<Vec<Handle<Mesh>>> {
        let label = match mesh_obj.name() {
            Some(name) if !name.is_empty() => format!("FbxMesh@{name}/Primitive"),
            _ => format!("FbxMesh{}/Primitive", mesh_obj.object_id().raw()),
        };
        trace!(
            "loading geometry mesh for node_id: {:?}",
            mesh_obj.object_node_id()
        );

        #[cfg(feature = "profile")]
        let _load_geometry_mesh = info_span!("load_geometry_mesh", label = &label).entered();

        #[cfg(feature = "profile")]
        let triangulate_mesh = info_span!("traingulate_mesh", label = &label).entered();

        let polygon_vertices = mesh_obj
            .polygon_vertices()
            .context("Failed to get polygon vertices")?;
        let triangle_pvi_indices = polygon_vertices
            .triangulate_each(triangulate::triangulate)
            .context("Triangulation failed")?;

        #[cfg(feature = "profile")]
        drop(triangulate_mesh);

        // TODO this seems to duplicate vertices from neighboring triangles. We shouldn't
        // do that and instead set the indice attribute of the Mesh properly.
        let get_position = |pos: Option<_>| -> Result<_, anyhow::Error> {
            let cpi = pos.ok_or_else(|| anyhow!("Failed to get control point index"))?;
            let point = polygon_vertices
                .control_point(cpi)
                .ok_or_else(|| anyhow!("Failed to get control point: cpi={:?}", cpi))?;
            Ok(DVec3::from((point.x, point.y, point.z)).as_vec3().into())
        };
        let positions = triangle_pvi_indices
            .iter_control_point_indices()
            .map(get_position)
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to reconstruct position vertices")?;

        debug!("Expand position lenght to {}", positions.len());

        let layer = mesh_obj
            .layers()
            .next()
            .ok_or_else(|| anyhow!("Failed to get layer"))?;

        let indices_per_material = || -> Result<_, anyhow::Error> {
            if num_materials == 0 {
                return Ok(None);
            };
            let mut indices_per_material = vec![Vec::new(); num_materials];
            let materials = layer
                .layer_element_entries()
                .find_map(|entry| match entry.typed_layer_element() {
                    Ok(TypedLayerElementHandle::Material(handle)) => Some(handle),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("Materials not found for mesh {:?}", mesh_obj))?
                .materials()
                .context("Failed to get materials")?;
            for tri_vi in triangle_pvi_indices.triangle_vertex_indices() {
                let local_material_index = materials
                    .material_index(&triangle_pvi_indices, tri_vi)
                    .context("Failed to get mesh-local material index")?
                    .to_u32();
                indices_per_material
                     .get_mut(local_material_index as usize)
                     .ok_or_else(|| {
                         anyhow!(
                             "FbxMesh-local material index out of range: num_materials={:?}, got={:?}",
                             num_materials,
                             local_material_index
                         )
                     })?
                     .push(tri_vi.to_usize() as u32);
            }
            Ok(Some(indices_per_material))
        };
        let normals = {
            let normals = layer
                .layer_element_entries()
                .find_map(|entry| match entry.typed_layer_element() {
                    Ok(TypedLayerElementHandle::Normal(handle)) => Some(handle),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("Failed to get normals"))?
                .normals()
                .context("Failed to get normals")?;
            let get_indices = |tri_vi| -> Result<_, anyhow::Error> {
                let v = normals.normal(&triangle_pvi_indices, tri_vi)?;
                Ok(DVec3::from((v.x, v.y, v.z)).as_vec3().into())
            };
            triangle_pvi_indices
                .triangle_vertex_indices()
                .map(get_indices)
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to reconstruct normals vertices")?
        };
        let uv = {
            let uv = layer
                .layer_element_entries()
                .find_map(|entry| match entry.typed_layer_element() {
                    Ok(TypedLayerElementHandle::Uv(handle)) => Some(handle),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("Failed to get UV"))?
                .uv()?;
            let get_indices = |tri_vi| -> Result<_, anyhow::Error> {
                let v = uv.uv(&triangle_pvi_indices, tri_vi)?;
                let fbx_uv_space = DVec2::from((v.x, v.y)).as_vec2();
                let bevy_uv_space = fbx_uv_space * Vec2::new(1.0, -1.0) + Vec2::new(0.0, 1.0);
                Ok(bevy_uv_space.into())
            };
            triangle_pvi_indices
                .triangle_vertex_indices()
                .map(get_indices)
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to reconstruct UV vertices")?
        };

        if uv.len() != positions.len() || uv.len() != normals.len() {
            bail!(
                "mismatched length of buffers: pos{} uv{} normals{}",
                positions.len(),
                uv.len(),
                normals.len(),
            );
        }

        // TODO: remove unused vertices from partial models
        // this is complicated, as it also requires updating the indices.

        // A single mesh may have multiple materials applied to a different subset of
        // its vertices. In the following code, we create a unique mesh per material
        // we found.
        let full_mesh_indices: Vec<_> = triangle_pvi_indices
            .triangle_vertex_indices()
            .map(|t| t.to_usize() as u32)
            .collect();
        let all_indices = if let Some(per_materials) = indices_per_material()? {
            per_materials
        } else {
            vec![full_mesh_indices.clone()]
        };

        debug!("Material count for {label}: {}", all_indices.len());

        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::all());
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_POSITION,
            VertexAttributeValues::Float32x3(positions),
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, VertexAttributeValues::Float32x2(uv));
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_NORMAL,
            VertexAttributeValues::Float32x3(normals),
        );
        mesh.insert_indices(Indices::U32(full_mesh_indices));
        mesh.generate_tangents()
            .context("Failed to generate tangents")?;

        let all_handles = all_indices
            .into_iter()
            .enumerate()
            .map(|(i, material_indices)| {
                debug!("Material {i} has {} vertices", material_indices.len());

                let mut material_mesh = mesh.clone();
                material_mesh.insert_indices(Indices::U32(material_indices));

                let label = format!("{label}{i}");

                let handle = self
                    .load_context
                    .add_labeled_asset(label.to_string(), material_mesh);
                self.scene.bevy_meshes.insert(handle.clone(), label);
                handle
            })
            .collect();
        Ok(all_handles)
    }

    // Note: FBX meshes can have multiple different materials, it's not just a mesh.
    // the FBX equivalent of a bevy Mesh is a geometry mesh
    async fn load_mesh(
        &mut self,
        mesh_obj: object::model::MeshHandle<'_>,
    ) -> anyhow::Result<FbxMesh> {
        let label = if let Some(name) = mesh_obj.name() {
            format!("FbxMesh@{name}")
        } else {
            format!("FbxMesh{}", mesh_obj.object_id().raw())
        };
        debug!("Loading FBX mesh: {label}");

        let bevy_obj = mesh_obj.geometry().context("Failed to get geometry")?;

        // async and iterators into for are necessary because of `async` `read_asset_bytes`
        // call in `load_video_clip`  that virally infect everything.
        // This can't even be ran in parallel, because we store already-encountered materials.
        let mut materials = Vec::new();
        for mat in mesh_obj.materials() {
            let mat = self.load_material(mat).await;
            let mat = mat.context("Failed to load materials for mesh")?;
            materials.push(mat);
        }
        let material_count = materials.len();
        if material_count == 0 {
            materials.push(Handle::default());
        }

        let bevy_mesh_handles = self
            .load_bevy_mesh(bevy_obj, material_count)
            .context("Failed to load geometry mesh")?;

        let mesh = FbxMesh {
            name: mesh_obj.name().map(Into::into),
            bevy_mesh_handles,
            materials,
        };

        let mesh_handle = self
            .load_context
            .add_labeled_asset(label.to_string(), mesh.clone());

        self.scene.meshes.insert(mesh_obj.object_id(), mesh_handle);

        Ok(mesh)
    }

    async fn load_video_clip(
        &mut self,
        video_clip_obj: object::video::ClipHandle<'_>,
    ) -> anyhow::Result<Image> {
        debug!("Loading texture image: {:?}", video_clip_obj.name());

        let relative_filename = video_clip_obj
            .relative_filename()
            .context("Failed to get relative filename of texture image")?;
        debug!("Relative filename: {:?}", relative_filename);

        let file_ext = Path::new(&relative_filename)
            .extension()
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        let image: Vec<u8> = if let Some(content) = video_clip_obj.content() {
            // TODO: the clone here is absolutely unnecessary, but there
            // is no way to reconciliate its lifetime with the other branch of
            // this if/else
            content.to_vec()
        } else {
            let parent = self.load_context.path().parent().unwrap();
            let clean_relative_filename = relative_filename.replace('\\', "/");
            let image_path = parent.join(clean_relative_filename);
            self.load_context.read_asset_bytes(image_path).await?
        };
        let is_srgb = false; // TODO
        let image = Image::from_buffer(
            image.as_slice(),
            ImageType::Extension(&file_ext),
            self.suported_compressed_formats,
            is_srgb,
            ImageSampler::Descriptor(ImageSamplerDescriptor {
                ..Default::default()
            }),
            RenderAssetUsages::all()
        );
        let image = image.context("Failed to read image buffer data")?;
        debug!(
            "Successfully loaded texture image: {:?}",
            video_clip_obj.name()
        );

        Ok(image)
    }

    async fn run_loader(
        &mut self,
        material_obj: object::material::MaterialHandle<'_>,
        MaterialLoader {
            static_load,
            dynamic_load,
            preprocess_textures,
            with_textures,
        }: MaterialLoader,
    ) -> anyhow::Result<Option<StandardMaterial>> {
        use crate::utils::fbx_extend::*;
        enum TextureSource<'a> {
            Processed(Image),
            Handle(TextureHandle<'a>),
        }
        let mut textures = HashMap::default();
        // code is a bit tricky so here is a rundown:
        // 1. Load all textures that are meant to be preprocessed by the
        //    MaterialLoader
        for &label in dynamic_load {
            if let Some(texture) = material_obj.load_texture(label) {
                let texture = self.get_texture(texture).await?;
                textures.insert(label, texture);
            }
        }
        preprocess_textures(material_obj, &mut textures);
        // 2. Put the loaded images and the non-preprocessed texture labels into an iterator
        let mut texture_handles = HashMap::with_capacity(textures.len() + static_load.len());
        let texture_handles_iter = textures
            .drain()
            .map(|(label, image)| (label, TextureSource::Processed(image)))
            .chain(static_load.iter().filter_map(|l| {
                material_obj
                    .load_texture(l)
                    .map(|te| (*l, TextureSource::Handle(te)))
            }));
        // 3. For each of those, create an image handle (with potential caching based on the texture name)
        for (label, texture) in texture_handles_iter {
            let handle_label = match texture {
                TextureSource::Handle(texture_handle) => match texture_handle.name() {
                    Some(name) if !name.is_empty() => format!("FbxTexture@{name}"),
                    _ => format!("FbxTexture{}", texture_handle.object_id().raw()),
                },
                TextureSource::Processed(_) => match material_obj.name() {
                    Some(name) if !name.is_empty() => format!("FbxTextureMat@{name}/{label}"),
                    _ => format!("FbxTextureMat{}/{label}", material_obj.object_id().raw()),
                },
            };

            // Either copy the already-created handle or create a new asset
            // for each image or texture to load.
            let handle = if let Some(handle) = self.scene.textures.get(&handle_label) {
                debug!("Already encountered texture: {label}, skipping");

                handle.clone()
            } else {
                let texture = match texture {
                    TextureSource::Processed(texture) => texture,
                    TextureSource::Handle(texture) => self.get_texture(texture).await?,
                };
                let handle = self
                    .load_context
                    .add_labeled_asset(handle_label.to_string(), texture);
                self.scene.textures.insert(handle_label, handle.clone());
                handle
            };
            texture_handles.insert(label, handle);
        }
        // 4. Call with all the texture handles
        Ok(with_textures(material_obj, texture_handles))
    }

    async fn get_texture(
        &mut self,
        texture_obj: object::texture::TextureHandle<'_>,
    ) -> anyhow::Result<Image> {
        let properties = texture_obj.properties();
        let address_mode_u = {
            let val = properties
                .wrap_mode_u_or_default()
                .context("Failed to load wrap mode for U axis")?;
            match val {
                WrapMode::Repeat => AddressMode::Repeat,
                WrapMode::Clamp => AddressMode::ClampToEdge,
            }
        };
        let address_mode_v = {
            let val = properties
                .wrap_mode_v_or_default()
                .context("Failed to load wrap mode for V axis")?;
            match val {
                WrapMode::Repeat => AddressMode::Repeat,
                WrapMode::Clamp => AddressMode::ClampToEdge,
            }
        };
        let video_clip_obj = texture_obj
            .video_clip()
            .context("No image data for texture object")?;

        let image: Result<Image, anyhow::Error> = self.load_video_clip(video_clip_obj).await;
        let mut image = image.context("Failed to load texture image")?;

        image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: address_mode_u.into(),
            address_mode_v: address_mode_v.into(),
            ..Default::default()
        });
        Ok(image)
    }

    async fn load_material(
        &mut self,
        material_obj: object::material::MaterialHandle<'_>,
    ) -> anyhow::Result<Handle<StandardMaterial>> {
        let label = match material_obj.name() {
            Some(name) if !name.is_empty() => format!("FbxMaterial@{name}"),
            _ => format!("FbxMaterial{}", material_obj.object_id().raw()),
        };
        if let Some(handle) = self.scene.materials.get(&label) {
            debug!("Already encountered material: {label}, skipping");

            return Ok(handle.clone_weak());
        }
        debug!("Loading FBX material: {label}");

        let mut material = None;
        let loaders = self.material_loaders.clone();
        for &loader in &loaders {
            if let Some(loader_material) = self.run_loader(material_obj, loader).await? {
                material = Some(loader_material);
                break;
            }
        }
        let material = material.context("None of the material loaders could load this material")?;
        let handle = self
            .load_context
            .add_labeled_asset(label.to_string(), material);
        debug!("Successfully loaded material: {label}");

        self.scene.materials.insert(label, handle.clone());
        Ok(handle)
    }
}

fn traverse_hierarchy(node: ModelHandle, hierarchy: &mut HashMap<ObjectId, FbxObject>) {
    #[cfg(feature = "profile")]
    let _hierarchy_span = info_span!("traverse_fbx_hierarchy").entered();

    traverse_hierarchy_rec(node, None, hierarchy);
    debug!("Tree has {} nodes", hierarchy.len());
    trace!("root: {:?}", node.object_node_id());
}
fn traverse_hierarchy_rec(
    node: ModelHandle,
    parent: Option<FbxTransform>,
    hierarchy: &mut HashMap<ObjectId, FbxObject>,
) -> bool {
    let name = node.name().map(|s| s.to_owned());
    let data = FbxTransform::from_node(node, parent);

    let mut mesh_leaf = false;
    node.child_models().for_each(|child| {
        mesh_leaf |= traverse_hierarchy_rec(*child, Some(data), hierarchy);
    });
    if node.subclass() == "Mesh" {
        mesh_leaf = true;
    }
    // Only keep nodes that have Mesh children
    // (ie defines something visible in the scene)
    // I've found some very unwindy FBX files with several thousand
    // nodes that served no practical purposes,
    // This also trims deformers and limb nodes, which we currently
    // do not support
    if mesh_leaf {
        let fbx_object = FbxObject {
            name,
            transform: data.as_local_transform(parent.as_ref().map(|p| p.global)),
            children: node.child_models().map(|c| c.object_id()).collect(),
        };
        hierarchy.insert(node.object_id(), fbx_object);
    }
    mesh_leaf
}
