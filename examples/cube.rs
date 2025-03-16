use bevy::{
    log::{Level, LogPlugin},
    prelude::*,
    render::camera::ScalingMode,
    window::WindowResolution,
};
use bevy_mod_fbx::FbxPlugin;

#[derive(Component)]
pub struct Spin;

fn main() {
    let mut app = App::new();

    app.add_plugins(
        DefaultPlugins
            .set(LogPlugin {
                level: Level::INFO,
                filter: "bevy_mod_fbx=trace,wgpu=warn".to_owned(),
                ..Default::default()
            })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "Spinning Cube".into(),
                    resolution: WindowResolution::new(756., 574.),
                    ..default()
                }),
                ..default()
            }),
    )
    .add_plugins(FbxPlugin)
    .add_systems(Startup, setup)
    .add_systems(Update, spin_cube);

    app.run();
}

fn spin_cube(time: Res<Time>, mut query: Query<&mut Transform, With<Spin>>) {
    for mut transform in query.iter_mut() {
        transform.rotate_local_y(0.3 * time.delta_secs());
        transform.rotate_local_x(0.3 * time.delta_secs());
        transform.rotate_local_z(0.3 * time.delta_secs());
    }
}

fn setup(mut cmd: Commands, asset_server: Res<AssetServer>) {
    // Orthographic camera
    cmd.spawn((
        Camera3d::default(),
        Transform::from_xyz(5.0, 5.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // light
    cmd.spawn((
        PointLight {
            intensity: 1000.0,
            ..default()
        },
        Transform::from_xyz(3.0, 8.0, 5.0),
    ));

    // Cube
    cmd.spawn((
        SceneRoot ( asset_server.load("cube.fbx#Scene")),
        Spin,
    ));
}
