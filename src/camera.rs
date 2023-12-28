use std::f32::consts::PI;

use bevy::{prelude::*, input::mouse::{MouseMotion, MouseWheel}, window::{PrimaryWindow, CursorGrabMode}};

use crate::Player;

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app
            .add_systems(Startup, setup_cursor)
            .add_systems(Update, (orbit_camera, apply_zoom));
    }
}

#[derive(Component)]
pub struct Camera {
    pub distance: f32,
    pub mouse_sensitivity: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            distance: 10.0,
            mouse_sensitivity: 0.5,
        }
    }
}

fn setup_cursor(
    mut window_query: Query<&mut Window, With<PrimaryWindow>>,
) {
    let mut window = window_query.get_single_mut().expect("not one window");

    window.cursor.visible = false;
    window.cursor.grab_mode = CursorGrabMode::Locked;
}

fn orbit_camera(
    window_query: Query<&Window, With<PrimaryWindow>>,
    mut cam_query: Query<(&mut Transform, &Camera)>,
    player_query: Query<&Transform, (With<Player>, Without<Camera>)>,
    mut mouse_event_reader: EventReader<MouseMotion>
) {
    let (mut cam_transform, cam) = cam_query.get_single_mut().expect("");
    let player_transform = player_query.get_single().expect("not one player");

    // sum all mouse motions since the last frame
    let mut mouse_delta = mouse_event_reader.read()
        .fold(Vec2::ZERO, |sum, i| sum + i.delta);

    // make sure the camera can't go inside the player
    if cam_transform.translation == player_transform.translation {
        cam_transform.translation.x += cam.distance;
    }

    // normalize mouse movements since they are relative to the 
    // screen size (in pixels)
    let window = window_query.get_single().expect("not one window");
    mouse_delta.x /= window.width();
    mouse_delta.y /= window.height();

    // bring in the mouse_sensitivity (changable)
    // and convert to radians
    mouse_delta.x *= cam.mouse_sensitivity * 2.0 * PI;
    mouse_delta.y *= cam.mouse_sensitivity * 2.0 * PI;

    // if the mouse goes up rotate the cam down
    let pitch = Quat::from_rotation_x(-mouse_delta.y);
    // if the mouse goes right, rotate the cam left
    let yaw = Quat::from_rotation_y(-mouse_delta.x);
    
    // apply yaw
    cam_transform.rotation = yaw * cam_transform.rotation;

    // apply pitch only if the camera doesn't too far
    if (cam_transform.rotation * pitch * Vec3::Y).y > 0.0 {
        cam_transform.rotation = cam_transform.rotation * pitch;
    }

    // rotate the cam around the player
    let rotation_matrix = Mat3::from_quat(cam_transform.rotation);
    cam_transform.translation = player_transform.translation 
        + rotation_matrix.mul_vec3(Vec3::new(0.0, 0.0, cam.distance));

}

fn apply_zoom(
    mut scroll_event_reader: EventReader<MouseWheel>,
    mut cam_query: Query<&mut Camera>,
) {
    // sum the scrolling events since last frame
    let delta = scroll_event_reader.read().fold(0.0, |sum, i| sum + i.y);

    if delta != 0.0 {
        let mut cam = cam_query.get_single_mut().expect("not one camera");
        cam.distance = f32::max(cam.distance - delta, 1.0);
    }
}
