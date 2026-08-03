use crate::{ChannelState, NetworkSnapshot, NodeInfo, NodeState};
use three_d::*;

pub struct VisualizerData {
    pub snapshots: Vec<NetworkSnapshot>,
    pub nodes: Vec<NodeInfo>,
    pub grid_rows: usize,
    pub grid_cols: usize,
}

pub struct Visualizer {
    data: VisualizerData,
    current_snapshot_index: usize,
}

impl Visualizer {
    pub fn new(data: VisualizerData) -> Self {
        Self {
            data,
            current_snapshot_index: 0,
        }
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
                            ui.label(format!(
                                "Snapshot: {}/{}",
                                self.current_snapshot_index + 1,
                                self.data.snapshots.len()
                            ));

                            // Use a custom slider with finer control
                            ui.horizontal(|ui| {
                                ui.label("Time:");
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

                            // Also add a drag value for precise control
                            ui.add(
                                egui::DragValue::new(&mut self.current_snapshot_index)
                                    .speed(1.0)
                                    .range(0..=self.data.snapshots.len().saturating_sub(1))
                                    .prefix("Snapshot: ")
                            );

                            ui.label(format!(
                                "Timestamp: {:?}",
                                self.data.snapshots[self.current_snapshot_index].timestamp
                            ));
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
                                        "Snapshot {}/{}",
                                        self.current_snapshot_index + 1,
                                        self.data.snapshots.len()
                                    );
                                }
                            }
                            Key::ArrowRight => {
                                if self.current_snapshot_index < self.data.snapshots.len() - 1 {
                                    self.current_snapshot_index += 1;
                                    println!(
                                        "Snapshot {}/{}",
                                        self.current_snapshot_index + 1,
                                        self.data.snapshots.len()
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

            // Build the 3D scene
            let objects = self.build_scene(&context);

            // Render scene
            frame_input
                .screen()
                .clear(ClearState::color_and_depth(0.1, 0.1, 0.15, 1.0, 1.0))
                .write(|| gui.render())
                .unwrap()
                .render(&camera, objects.iter().map(|o| o as &dyn Object), &[&light as &dyn Light, &ambient as &dyn Light]);

            FrameOutput::default()
        });
    }

    fn build_scene(&self, context: &Context) -> Vec<Gm<Mesh, PhysicalMaterial>> {
        let mut objects = Vec::new();

        // Return empty scene if no snapshots available
        if self.data.snapshots.is_empty() {
            return objects;
        }

        let snapshot = &self.data.snapshots[self.current_snapshot_index];

        // Render nodes (as positions in the grid)
        for (node_idx, _node_info) in self.data.nodes.iter().enumerate() {
            let (row, col) = self.node_idx_to_grid_pos(node_idx);
            let node_state = &snapshot.nodes[node_idx];

            // Render node as a small cube at grid position
            let node_mesh = self.create_node_mesh(context, row, col, node_state);
            objects.push(node_mesh);

            // Render LEDs extending upward
            let led_objects = self.create_led_meshes(context, row, col, node_state);
            objects.extend(led_objects);
        }

        // Render channels
        let channel_objects = self.create_channel_meshes(context, snapshot);
        objects.extend(channel_objects);

        objects
    }

    fn node_idx_to_grid_pos(&self, idx: usize) -> (usize, usize) {
        let row = idx / self.data.grid_cols;
        let col = idx % self.data.grid_cols;
        (row, col)
    }

    fn create_node_mesh(
        &self,
        context: &Context,
        row: usize,
        col: usize,
        _node_state: &NodeState,
    ) -> Gm<Mesh, PhysicalMaterial> {
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
        mesh
    }

    fn create_led_meshes(
        &self,
        context: &Context,
        row: usize,
        col: usize,
        node_state: &NodeState,
    ) -> Vec<Gm<Mesh, PhysicalMaterial>> {
        let base_position = vec3(col as f32, row as f32, 0.0);
        let led_count = node_state.colors.len().min(20); // Limit displayed LEDs
        let led_spacing = 0.2;

        node_state.colors[..led_count]
            .iter()
            .enumerate()
            .map(|(i, color)| {
                let position = base_position + vec3(0.0, 0.0, (i as f32 + 1.0) * led_spacing);
                let mut mesh = Gm::new(
                    Mesh::new(context, &CpuMesh::sphere(2)),
                    PhysicalMaterial::new_opaque(
                        context,
                        &CpuMaterial {
                            albedo: Srgba::new(color.r, color.g, color.b, 255),
                            ..Default::default()
                        },
                    ),
                );
                mesh.set_transformation(Mat4::from_translation(position) * Mat4::from_scale(0.02));
                mesh
            })
            .collect()
    }

    fn create_channel_meshes(
        &self,
        context: &Context,
        snapshot: &NetworkSnapshot,
    ) -> Vec<Gm<Mesh, PhysicalMaterial>> {
        let mut objects = Vec::new();

        for (node_idx, node_info) in self.data.nodes.iter().enumerate() {
            let (row, col) = self.node_idx_to_grid_pos(node_idx);
            let position = vec3(col as f32, row as f32, 0.0);

            // Draw channels to neighbors
            if let Some(channel_id) = node_info.east {
                if col + 1 < self.data.grid_cols {
                    let neighbor_pos = vec3((col + 1) as f32, row as f32, 0.0);
                    let channel_mesh =
                        self.create_channel_line(context, position, neighbor_pos, &snapshot.channels[channel_id]);
                    objects.push(channel_mesh);
                }
            }

            if let Some(channel_id) = node_info.south {
                if row + 1 < self.data.grid_rows {
                    let neighbor_pos = vec3(col as f32, (row + 1) as f32, 0.0);
                    let channel_mesh =
                        self.create_channel_line(context, position, neighbor_pos, &snapshot.channels[channel_id]);
                    objects.push(channel_mesh);
                }
            }
        }

        objects
    }

    fn create_channel_line(
        &self,
        context: &Context,
        start: Vec3,
        end: Vec3,
        channel_state: &ChannelState,
    ) -> Gm<Mesh, PhysicalMaterial> {
        let direction = end - start;
        let length = direction.magnitude();
        let midpoint = start + direction * 0.5;

        // Determine color based on state
        let color = if channel_state.conflict {
            Srgba::RED
        } else if channel_state.counter > 0 {
            Srgba::new(255, 255, 0, 255) // Yellow
        } else {
            Srgba::new(100, 100, 100, 255)
        };

        // Create cylinder along direction
        let rotation = rotation_matrix_from_dir_to_dir(vec3(0.0, 1.0, 0.0), direction.normalize());

        let mut mesh = Gm::new(
            Mesh::new(context, &CpuMesh::cylinder(3)),
            PhysicalMaterial::new_opaque(
                context,
                &CpuMaterial {
                    albedo: color,
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
