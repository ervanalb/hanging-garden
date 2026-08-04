use crate::{NetworkSnapshot, NodeInfo};
use three_d::*;

pub struct VisualizerData {
    pub snapshots: Vec<NetworkSnapshot>,
    pub nodes: Vec<NodeInfo>,
    pub grid_rows: usize,
    pub grid_cols: usize,
}

struct SceneObjects {
    // Static geometry - created once
    node_meshes: Vec<Gm<Mesh, PhysicalMaterial>>,
    led_meshes: Vec<Vec<Gm<Mesh, PhysicalMaterial>>>, // Per node, per LED
    channel_meshes: Vec<Gm<Mesh, PhysicalMaterial>>,  // One per channel
    channel_info: Vec<(usize, Vec3, Vec3)>, // (channel_id, start, end) for each channel mesh
}

pub struct Visualizer {
    data: VisualizerData,
    current_snapshot_index: usize,
    scene_objects: Option<SceneObjects>,
}

impl Visualizer {
    pub fn new(data: VisualizerData) -> Self {
        Self {
            data,
            current_snapshot_index: 0,
            scene_objects: None,
        }
    }

    fn get_snapshot_time(&self, index: usize) -> f64 {
        if self.data.snapshots.is_empty() {
            return 0.0;
        }
        let start = self.data.snapshots[0].timestamp;
        let current = self.data.snapshots[index].timestamp;
        (current - start).as_secs_f64()
    }

    pub fn run(mut self) {
        let window = Window::new(WindowSettings {
            title: "Network Simulator".to_string(),
            max_size: Some((1920, 1080)),
            ..Default::default()
        })
        .unwrap();

        let context = window.gl();

        let mut camera = Camera::new_perspective(
            window.viewport(),
            vec3(
                self.data.grid_cols as f32 * 0.5,
                self.data.grid_rows as f32 * 0.5,
                self.data.grid_rows.max(self.data.grid_cols) as f32 * 2.0,
            ),
            vec3(
                self.data.grid_cols as f32 * 0.5,
                self.data.grid_rows as f32 * 0.5,
                0.0,
            ),
            vec3(0.0, 1.0, 0.0),
            degrees(45.0),
            0.1,
            1000.0,
        );

        let mut orbit_control = OrbitControl::new(
            camera.target(),
            0.1,
            self.data.grid_rows.max(self.data.grid_cols) as f32 * 10.0,
        );

        // Lighting
        let light = DirectionalLight::new(&context, 2.0, Srgba::WHITE, vec3(0.0, -1.0, -1.0));
        let ambient = AmbientLight::new(&context, 0.4, Srgba::WHITE);

        // Create GUI for controls
        let mut gui = three_d::GUI::new(&context);

        // Build static scene geometry once
        if !self.data.snapshots.is_empty() {
            self.scene_objects = Some(self.build_static_scene(&context));
        }

        window.render_loop(move |mut frame_input| {
            camera.set_viewport(frame_input.viewport);

            // Build UI first to capture events
            let mut panel_width = 0.0;
            let mut egui_consumed_events = false;
            gui.update(
                &mut frame_input.events,
                frame_input.accumulated_time,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                |gui_context| {
                    // Only check if pointer is over egui (for mouse events, not keyboard)
                    egui_consumed_events = gui_context.egui_wants_pointer_input()
                        || gui_context.is_pointer_over_egui();
                    use three_d::egui::*;
                    Panel::left("side_panel").show_inside(gui_context, |ui| {
                        ui.heading("Network Simulator");
                        ui.separator();

                        if self.data.snapshots.is_empty() {
                            ui.label("No snapshots available");
                        } else {
                            let current_time = self.get_snapshot_time(self.current_snapshot_index);

                            ui.label(format!(
                                "Event: {}/{} | Time: {:.3}s",
                                self.current_snapshot_index + 1,
                                self.data.snapshots.len(),
                                current_time
                            ));

                            ui.separator();
                            ui.label("Navigate by Event:");

                            // Event slider with buttons
                            ui.horizontal(|ui| {
                                if ui.button("◀").clicked() && self.current_snapshot_index > 0 {
                                    self.current_snapshot_index -= 1;
                                }
                                ui.add(
                                    Slider::new(
                                        &mut self.current_snapshot_index,
                                        0..=self.data.snapshots.len().saturating_sub(1),
                                    )
                                    .show_value(false)
                                );
                                if ui.button("▶").clicked() && self.current_snapshot_index < self.data.snapshots.len() - 1 {
                                    self.current_snapshot_index += 1;
                                }
                            });

                            // Event drag value
                            ui.add(
                                egui::DragValue::new(&mut self.current_snapshot_index)
                                    .speed(1.0)
                                    .range(0..=self.data.snapshots.len().saturating_sub(1))
                                    .prefix("Event #")
                            );

                            ui.separator();
                            ui.label("Navigate by Time:");

                            // Build a list of all event timestamps for the custom slider
                            let event_times: Vec<f64> = (0..self.data.snapshots.len())
                                .map(|i| self.get_snapshot_time(i))
                                .collect();

                            // Time slider that snaps to event timestamps
                            let mut selected_time_index = self.current_snapshot_index;
                            if ui.add(
                                Slider::new(&mut selected_time_index, 0..=self.data.snapshots.len().saturating_sub(1))
                                    .custom_formatter(|val, _| {
                                        let idx = val as usize;
                                        format!("{:.3}s", event_times.get(idx).unwrap_or(&0.0))
                                    })
                                    .show_value(true)
                            ).changed() {
                                self.current_snapshot_index = selected_time_index;
                            }

                            // Display current time as read-only
                            ui.label(format!("Current time: {:.3}s", current_time));
                        }

                        ui.separator();
                        ui.label("Camera Controls:");
                        ui.label("  Mouse drag: Rotate");
                        ui.label("  Scroll: Zoom");
                        ui.label("  Arrow keys: Navigate");
                    });
                    panel_width = gui_context.globally_used_rect().width();
                },
            );

            // Handle keyboard input for snapshot navigation (only if egui didn't consume it)
            if !egui_consumed_events {
                for event in frame_input.events.iter() {
                    match event {
                        Event::KeyPress { kind, .. } => match kind {
                            Key::ArrowLeft => {
                                if self.current_snapshot_index > 0 {
                                    self.current_snapshot_index -= 1;
                                    println!(
                                        "Event {}/{} | Time: {:.3}s",
                                        self.current_snapshot_index + 1,
                                        self.data.snapshots.len(),
                                        self.get_snapshot_time(self.current_snapshot_index)
                                    );
                                }
                            }
                            Key::ArrowRight => {
                                if self.current_snapshot_index < self.data.snapshots.len() - 1 {
                                    self.current_snapshot_index += 1;
                                    println!(
                                        "Event {}/{} | Time: {:.3}s",
                                        self.current_snapshot_index + 1,
                                        self.data.snapshots.len(),
                                        self.get_snapshot_time(self.current_snapshot_index)
                                    );
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }

                // Only handle camera controls if egui didn't consume events
                orbit_control.handle_events(&mut camera, &mut frame_input.events);
            }

            // Adjust camera viewport to not overlap with GUI
            let mut viewport = frame_input.viewport;
            viewport.x = (panel_width * frame_input.device_pixel_ratio) as i32;
            viewport.width =
                frame_input.viewport.width - (panel_width * frame_input.device_pixel_ratio) as u32;
            camera.set_viewport(viewport);

            // Update colors based on current snapshot
            if !self.data.snapshots.is_empty() {
                self.update_scene_colors();
            }

            // Collect all objects for rendering
            let objects: Vec<&dyn Object> = if let Some(scene) = &self.scene_objects {
                let mut objs: Vec<&dyn Object> = Vec::new();
                for mesh in &scene.node_meshes {
                    objs.push(mesh as &dyn Object);
                }
                for node_leds in &scene.led_meshes {
                    for led in node_leds {
                        objs.push(led as &dyn Object);
                    }
                }
                for mesh in &scene.channel_meshes {
                    objs.push(mesh as &dyn Object);
                }
                objs
            } else {
                Vec::new()
            };

            // Render scene
            frame_input
                .screen()
                .clear(ClearState::color_and_depth(0.1, 0.1, 0.15, 1.0, 1.0))
                .write(|| gui.render())
                .unwrap()
                .render(&camera, objects.into_iter(), &[&light as &dyn Light, &ambient as &dyn Light]);

            FrameOutput::default()
        });
    }

    fn build_static_scene(&self, context: &Context) -> SceneObjects {
        let mut node_meshes = Vec::new();
        let mut led_meshes = Vec::new();
        let mut channel_meshes = Vec::new();
        let mut channel_info = Vec::new();

        // Create static node geometry
        for node_idx in 0..self.data.nodes.len() {
            let (row, col) = self.node_idx_to_grid_pos(node_idx);
            let position = vec3(col as f32, row as f32, 0.0);

            let mut mesh = Gm::new(
                Mesh::new(context, &CpuMesh::cube()),
                PhysicalMaterial::new_opaque(
                    context,
                    &CpuMaterial {
                        albedo: Srgba::new(150, 150, 150, 255),
                        ..Default::default()
                    },
                ),
            );
            mesh.set_transformation(Mat4::from_translation(position) * Mat4::from_scale(0.1));
            node_meshes.push(mesh);

            // Create static LED geometry (max 20 per node)
            let base_position = vec3(col as f32, row as f32, 0.0);
            let led_spacing = 0.2;
            let max_leds = 20;

            let mut node_leds = Vec::new();
            for i in 0..max_leds {
                let position = base_position + vec3(0.0, 0.0, (i as f32 + 1.0) * led_spacing);
                let mut mesh = Gm::new(
                    Mesh::new(context, &CpuMesh::sphere(2)),
                    PhysicalMaterial::new_opaque(
                        context,
                        &CpuMaterial {
                            albedo: Srgba::BLACK, // Will be updated each frame
                            ..Default::default()
                        },
                    ),
                );
                mesh.set_transformation(Mat4::from_translation(position) * Mat4::from_scale(0.02));
                node_leds.push(mesh);
            }
            led_meshes.push(node_leds);
        }

        // Create static channel geometry
        for (node_idx, node_info) in self.data.nodes.iter().enumerate() {
            let (row, col) = self.node_idx_to_grid_pos(node_idx);
            let position = vec3(col as f32, row as f32, 0.0);

            // East channel
            if let Some(channel_id) = node_info.east {
                if col + 1 < self.data.grid_cols {
                    let neighbor_pos = vec3((col + 1) as f32, row as f32, 0.0);
                    let mesh = self.create_static_channel_mesh(context, position, neighbor_pos);
                    channel_info.push((channel_id, position, neighbor_pos));
                    channel_meshes.push(mesh);
                }
            }

            // South channel
            if let Some(channel_id) = node_info.south {
                if row + 1 < self.data.grid_rows {
                    let neighbor_pos = vec3(col as f32, (row + 1) as f32, 0.0);
                    let mesh = self.create_static_channel_mesh(context, position, neighbor_pos);
                    channel_info.push((channel_id, position, neighbor_pos));
                    channel_meshes.push(mesh);
                }
            }
        }

        SceneObjects {
            node_meshes,
            led_meshes,
            channel_meshes,
            channel_info,
        }
    }

    fn update_scene_colors(&mut self) {
        let snapshot = &self.data.snapshots[self.current_snapshot_index];

        if let Some(scene) = &mut self.scene_objects {
            // Update LED colors
            for (node_idx, node_leds) in scene.led_meshes.iter_mut().enumerate() {
                let node_state = &snapshot.nodes[node_idx];
                for (led_idx, led_mesh) in node_leds.iter_mut().enumerate() {
                    if led_idx < node_state.colors.len() {
                        let color = &node_state.colors[led_idx];
                        led_mesh.material.albedo = Srgba::new(color.r, color.g, color.b, 255);
                    } else {
                        // Hide unused LEDs by making them black/invisible
                        led_mesh.material.albedo = Srgba::BLACK;
                    }
                }
            }

            // Update channel colors
            for (mesh_idx, (channel_id, _, _)) in scene.channel_info.iter().enumerate() {
                let channel_state = &snapshot.channels[*channel_id];
                let color = if channel_state.conflict {
                    Srgba::RED
                } else if channel_state.counter > 0 {
                    Srgba::new(255, 255, 0, 255) // Yellow
                } else {
                    Srgba::new(100, 100, 100, 255) // Gray
                };
                scene.channel_meshes[mesh_idx].material.albedo = color;
            }
        }
    }

    fn node_idx_to_grid_pos(&self, idx: usize) -> (usize, usize) {
        let row = idx / self.data.grid_cols;
        let col = idx % self.data.grid_cols;
        (row, col)
    }

    fn create_static_channel_mesh(
        &self,
        context: &Context,
        start: Vec3,
        end: Vec3,
    ) -> Gm<Mesh, PhysicalMaterial> {
        let direction = end - start;
        let length = direction.magnitude();
        let midpoint = start + direction * 0.5;

        // Create cylinder along direction
        let rotation = rotation_matrix_from_dir_to_dir(vec3(0.0, 1.0, 0.0), direction.normalize());

        let mut mesh = Gm::new(
            Mesh::new(context, &CpuMesh::cylinder(3)),
            PhysicalMaterial::new_opaque(
                context,
                &CpuMaterial {
                    albedo: Srgba::new(100, 100, 100, 255), // Default gray, will be updated
                    ..Default::default()
                },
            ),
        );

        mesh.set_transformation(
            Mat4::from_translation(midpoint)
                * rotation
                * Mat4::from_nonuniform_scale(0.02, length * 0.5, 0.02),
        );
        mesh
    }
}
