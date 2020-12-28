mod command;
mod compiler;

/// This module houses all the dynamically created meshes used by the drawing code to fulfill user commands.
mod geom;

mod geom_buf;
mod instruction;
mod key;

pub use self::{command::Command, compiler::Compiler};

use {
    self::{
        command::SunlightCommand,
        compiler::VertexBuffers,
        geom::{LINE_STRIDE, POINT_LIGHT, RECT_LIGHT_STRIDE, SPOTLIGHT_STRIDE},
        geom_buf::GeometryBuffer,
        instruction::{
            DataComputeInstruction, DataCopyInstruction, DataTransferInstruction,
            DataWriteInstruction, DataWriteRefInstruction, Instruction, LightBindInstruction,
            LineDrawInstruction, MeshBindInstruction, MeshDrawInstruction,
            PointLightDrawInstruction, RectLightDrawInstruction, SpotlightDrawInstruction,
            VertexAttrsDescriptorsInstruction,
        },
    },
    super::Op,
    crate::{
        camera::Camera,
        color::AlphaColor,
        gpu::{
            data::{CopyRange, Mapping},
            def::{
                push_const::{
                    CalcVertexAttrsPushConsts, Mat4PushConst, PointLightPushConsts,
                    RectLightPushConsts, SkydomeFragmentPushConsts, SkydomeVertexPushConsts,
                    SunlightPushConsts,
                },
                CalcVertexAttrsComputeMode, Compute, ComputeMode, DrawRenderPassMode, Graphics,
                GraphicsMode, RenderPassMode,
            },
            driver::{
                bind_compute_descriptor_set, bind_graphics_descriptor_set, CommandPool, Device,
                Driver, Fence, Framebuffer2d,
            },
            pool::{Lease, Pool},
            BitmapRef, Data, Texture2d, TextureRef,
        },
        math::{Coord, Mat3, Mat4, Quat, Vec3},
    },
    gfx_hal::{
        adapter::PhysicalDevice as _,
        buffer::{Access as BufferAccess, IndexBufferView, SubRange},
        command::{
            ClearColor, ClearDepthStencil, ClearValue, CommandBuffer as _, CommandBufferFlags,
            ImageCopy, Level, SubpassContents,
        },
        device::Device as _,
        format::Aspects,
        image::{
            Access as ImageAccess, Layout, Offset, SubresourceLayers, SubresourceRange, ViewKind,
        },
        pool::CommandPool as _,
        pso::{Descriptor, DescriptorSetWrite, PipelineStage, ShaderStageFlags, Viewport},
        queue::{CommandQueue as _, Submission},
        Backend,
    },
    gfx_impl::Backend as _Backend,
    std::{
        any::Any,
        cmp::Ordering,
        hash::{Hash, Hasher},
        iter::{empty, once},
    },
};

pub struct DrawOp {
    cmd_buf: <_Backend as Backend>::CommandBuffer,
    cmd_pool: Lease<CommandPool>,
    compiler: Option<Lease<Compiler>>,
    compute_u16_vertex_attrs: Option<Lease<Compute>>,
    compute_u16_skin_vertex_attrs: Option<Lease<Compute>>,
    compute_u32_vertex_attrs: Option<Lease<Compute>>,
    compute_u32_skin_vertex_attrs: Option<Lease<Compute>>,
    driver: Driver,
    dst: Texture2d,
    dst_preserve: bool,
    fence: Lease<Fence>,
    frame_buf: Option<(Framebuffer2d, RenderPassMode)>,
    geom_buf: GeometryBuffer,
    graphics_line: Option<Lease<Graphics>>,
    graphics_mesh: Option<Lease<Graphics>>,
    graphics_mesh_anim: Option<Lease<Graphics>>,
    graphics_point_light: Option<Lease<Graphics>>,
    graphics_rect_light: Option<Lease<Graphics>>,
    graphics_skydome: Option<Lease<Graphics>>,
    graphics_spotlight: Option<Lease<Graphics>>,
    graphics_sunlight: Option<Lease<Graphics>>,

    #[cfg(feature = "debug-names")]
    name: String,

    pool: Option<Lease<Pool>>,
    skydome: Option<(Skydome, Lease<Data>, u64, bool)>,
}

impl DrawOp {
    /// # Safety
    /// None
    #[must_use]
    pub fn new(
        #[cfg(feature = "debug-names")] name: &str,
        driver: &Driver,
        mut pool: Lease<Pool>,
        dst: &Texture2d,
    ) -> Self {
        // Allocate the command buffer
        let family = Device::queue_family(&driver.borrow());
        let mut cmd_pool = pool.cmd_pool(driver, family);

        // The geometry buffer will share size and output format with the destination texture
        let (dims, fmt) = {
            let dst = dst.borrow();
            (dst.dims(), dst.format())
        };

        Self {
            cmd_buf: unsafe { cmd_pool.allocate_one(Level::Primary) },
            cmd_pool,
            compiler: None,
            compute_u16_vertex_attrs: None,
            compute_u16_skin_vertex_attrs: None,
            compute_u32_vertex_attrs: None,
            compute_u32_skin_vertex_attrs: None,
            driver: Driver::clone(driver),
            dst: TextureRef::clone(dst),
            dst_preserve: false,
            fence: pool.fence(
                #[cfg(feature = "debug-names")]
                name,
                driver,
            ),
            frame_buf: None,
            geom_buf: GeometryBuffer::new(
                #[cfg(feature = "debug-names")]
                name,
                driver,
                &mut pool,
                dims,
                fmt,
            ),
            graphics_line: None,
            graphics_mesh: None,
            graphics_mesh_anim: None,
            graphics_point_light: None,
            graphics_rect_light: None,
            graphics_skydome: None,
            graphics_spotlight: None,
            graphics_sunlight: None,

            #[cfg(feature = "debug-names")]
            name: name.to_owned(),

            pool: Some(pool),
            skydome: None,
        }
    }

    /// Preserves the contents of the destination texture. Without calling this function the existing
    /// contents of the destination texture will not be composited into the final result.
    #[must_use]
    pub fn with_preserve(&mut self, val: bool) -> &mut Self {
        self.dst_preserve = val;
        self
    }

    /// Draws the given skydome as a pre-pass before the geometry and lighting.
    #[must_use]
    pub fn with_skydome(&mut self, val: &Skydome) -> &mut Self {
        // Either take the existing skydome buffer or get a new one (ignoring the old skydome)
        let (buf, buf_len, write) = if let Some((_, buf, buf_len, write)) = self.skydome.take() {
            (buf, buf_len, write)
        } else {
            let pool = self.pool.as_mut().unwrap();
            let (mut buf, buf_len, data) = pool.skydome(
                #[cfg(feature = "debug-names")]
                &self.name,
                &self.driver,
            );

            // Fill the skydome buffer if it is brand new (data was provided)
            if let Some(data) = data {
                let mut mapped_range = buf.map_range_mut(0..data.len() as _).unwrap();
                mapped_range.copy_from_slice(&data);
                Mapping::flush(&mut mapped_range).unwrap();
            }

            (buf, buf_len, data.is_some())
        };

        self.skydome = Some((val.clone(), buf, buf_len, write));
        self
    }

    pub fn record(&mut self, camera: &impl Camera, cmds: &mut [Command]) {
        let skydome_subpass_idx = 0;
        let fill_geom_buf_subpass_idx = self.fill_geom_buf_subpass_idx();
        let mut pool = self.pool.as_mut().unwrap();

        // Use a compiler to figure out rendering instructions without allocating
        // memory per rendering command. The compiler caches code between frames.
        let mut compiler = pool.compiler();
        {
            let mut instrs = compiler.compile(
                #[cfg(feature = "debug-names")]
                &self.name,
                &self.driver,
                &mut pool,
                camera,
                cmds,
            );

            let render_pass_mode = {
                let dst = self.dst.borrow();
                let dims = dst.dims();
                let color_metal = self.geom_buf.color_metal.borrow();
                let depth = self.geom_buf.depth.borrow();
                let light = self.geom_buf.light.borrow();
                let normal_rough = self.geom_buf.normal_rough.borrow();
                let output = self.geom_buf.output.borrow();
                let draw_mode = DrawRenderPassMode {
                    depth: depth.format(),
                    geom_buf: color_metal.format(),
                    light: light.format(),
                    output: output.format(),
                    skydome: self.skydome.is_some(),
                    post_fx: instrs.contains_lines(),
                };
                let render_pass_mode = RenderPassMode::Draw(draw_mode);
                let render_pass = pool.render_pass(&self.driver, render_pass_mode);

                // Setup the framebuffer
                self.frame_buf = Some((
                    Framebuffer2d::new(
                        #[cfg(feature = "debug-names")]
                        &self.name,
                        &self.driver,
                        render_pass,
                        vec![
                            color_metal.as_default_view().as_ref(),
                            normal_rough.as_default_view().as_ref(),
                            light.as_default_view().as_ref(),
                            output.as_default_view().as_ref(),
                            depth
                                .as_view(
                                    ViewKind::D2,
                                    draw_mode.depth,
                                    Default::default(),
                                    SubresourceRange {
                                        aspects: Aspects::DEPTH,
                                        ..Default::default()
                                    },
                                )
                                .as_ref(),
                        ],
                        dims,
                    ),
                    render_pass_mode,
                ));
                render_pass_mode
            };

            if let Some((skydome, _, _, _)) = &self.skydome {
                let graphics = pool.graphics_desc_sets(
                    #[cfg(feature = "debug-names")]
                    &self.name,
                    &self.driver,
                    render_pass_mode,
                    skydome_subpass_idx,
                    GraphicsMode::Skydome,
                    1,
                );
                let device = self.driver.borrow();

                unsafe {
                    Self::write_skydome_descriptors(&device, &graphics, skydome);
                }

                self.graphics_skydome = Some(graphics);
            }

            {
                // Material descriptors for PBR rendering (Color+Normal+Metal/Rough)
                let descriptors = instrs.materials();
                let desc_sets = descriptors.len();
                if desc_sets > 0 {
                    let graphics = pool.graphics_desc_sets(
                        #[cfg(feature = "debug-names")]
                        &self.name,
                        &self.driver,
                        render_pass_mode,
                        fill_geom_buf_subpass_idx,
                        GraphicsMode::DrawMesh,
                        desc_sets,
                    );
                    let device = self.driver.borrow();

                    unsafe {
                        Self::write_material_descriptors(&device, &graphics, descriptors);
                    }

                    self.graphics_mesh = Some(graphics);
                }

                // Buffer descriptors for calculation of u16-indexed vertex attributes
                let descriptors = instrs.u16_vertex_bufs();
                let desc_sets = descriptors.len();
                if desc_sets > 0 {
                    let compute = pool.compute_desc_sets(
                        #[cfg(feature = "debug-names")]
                        &self.name,
                        &self.driver,
                        ComputeMode::CalcVertexAttrs(CalcVertexAttrsComputeMode::U16),
                        desc_sets,
                    );
                    let device = self.driver.borrow();

                    unsafe {
                        Self::write_vertex_descriptors(&device, &compute, descriptors);
                    }

                    self.compute_u16_vertex_attrs = Some(compute);
                }

                // Buffer descriptors for calculation of u16-indexed skinned vertex attributes
                let descriptors = instrs.u16_skin_vertex_bufs();
                let desc_sets = descriptors.len();
                if desc_sets > 0 {
                    let compute = pool.compute_desc_sets(
                        #[cfg(feature = "debug-names")]
                        &self.name,
                        &self.driver,
                        ComputeMode::CalcVertexAttrs(CalcVertexAttrsComputeMode::U16_SKIN),
                        desc_sets,
                    );
                    let device = self.driver.borrow();

                    unsafe {
                        Self::write_vertex_descriptors(&device, &compute, descriptors);
                    }

                    self.compute_u16_skin_vertex_attrs = Some(compute);
                }

                // Buffer descriptors for calculation of u32-indexed vertex attributes
                let descriptors = instrs.u32_vertex_bufs();
                let desc_sets = descriptors.len();
                if desc_sets > 0 {
                    let compute = pool.compute_desc_sets(
                        #[cfg(feature = "debug-names")]
                        &self.name,
                        &self.driver,
                        ComputeMode::CalcVertexAttrs(CalcVertexAttrsComputeMode::U32),
                        desc_sets,
                    );
                    let device = self.driver.borrow();

                    unsafe {
                        Self::write_vertex_descriptors(&device, &compute, descriptors);
                    }

                    self.compute_u32_vertex_attrs = Some(compute);
                }

                // Buffer descriptors for calculation of u32-indexed skinned vertex attributes
                let descriptors = instrs.u32_skin_vertex_bufs();
                let desc_sets = descriptors.len();
                if desc_sets > 0 {
                    let compute = pool.compute_desc_sets(
                        #[cfg(feature = "debug-names")]
                        &self.name,
                        &self.driver,
                        ComputeMode::CalcVertexAttrs(CalcVertexAttrsComputeMode::U32_SKIN),
                        desc_sets,
                    );
                    let device = self.driver.borrow();

                    unsafe {
                        Self::write_vertex_descriptors(&device, &compute, descriptors);
                    }

                    self.compute_u32_skin_vertex_attrs = Some(compute);
                }
            }

            let proj = camera.projection();
            let view = camera.view();
            let view_proj = proj * view;
            let dims: Coord = self.dst.borrow().dims().into();
            let viewport = Viewport {
                rect: dims.as_rect_at(Coord::ZERO),
                depth: 0.0..1.0,
            };

            unsafe {
                self.submit_begin(&viewport);

                // Handle Skydome pre-fx
                if let Some((_, _, _, write)) = &mut self.skydome {
                    // Brand new skydomes from the pool must be written before use
                    if *write {
                        *write = false;
                        self.submit_skydome_write();
                    }

                    self.submit_skydome(&viewport, view);
                }

                while let Some(instr) = instrs.next() {
                    match instr {
                        Instruction::DataTransfer(instr) => self.submit_data_transfer(instr),
                        Instruction::IndexWriteRef(instr) => self.submit_index_write_ref(instr),
                        Instruction::LightBegin => self.submit_light_begin(),
                        Instruction::LightBind(instr) => self.submit_light_bind(instr),
                        Instruction::LineDraw(instr) => {
                            self.submit_lines(instr, &viewport, view_proj)
                        }
                        Instruction::MeshBegin => self.submit_mesh_begin(&viewport),
                        Instruction::MeshBind(instr) => self.submit_mesh_bind(instr),
                        Instruction::MeshDescriptors(set) => self.submit_mesh_descriptors(set),
                        Instruction::MeshDraw(instr) => self.submit_mesh(instr, view_proj),
                        Instruction::PointLightDraw(instr) => {
                            self.submit_point_lights(instr, &viewport, view_proj)
                        }
                        Instruction::RectLightBegin => self.submit_rect_light_begin(&viewport),
                        Instruction::RectLightDraw(instr) => {
                            self.submit_rect_light(instr, view_proj)
                        }
                        Instruction::SpotlightBegin => self.submit_spotlight_begin(&viewport),
                        Instruction::SpotlightDraw(instr) => {
                            self.submit_spotlight(instr, view_proj)
                        }
                        Instruction::SunlightDraw(instr) => self.submit_sunlights(instr, &viewport),
                        Instruction::VertexAttrsBegin(instr) => {
                            self.submit_vertex_attrs_begin(instr)
                        }
                        Instruction::VertexAttrsCalc(instr) => self.submit_vertex_attrs_calc(instr),
                        Instruction::VertexAttrsDescriptors(instr) => {
                            self.submit_vertex_attrs_descriptors(instr)
                        }
                        Instruction::VertexCopy(instr) => self.submit_vertex_copies(instr),
                        Instruction::VertexWrite(instr) => self.submit_vertex_write(instr),
                        Instruction::VertexWriteRef(instr) => self.submit_vertex_write_ref(instr),
                    }
                }

                // TODO: Submit post-fx here; tone mapping/lens aberrations

                self.submit_finish();
            }
        }

        self.compiler = Some(compiler);
    }

    fn fill_geom_buf_subpass_idx(&self) -> u8 {
        self.skydome.is_some() as u8
    }

    fn accum_light_subpass_idx(&self) -> u8 {
        1 + self.skydome.is_some() as u8
    }

    fn post_fx_subpass_idx(&self) -> u8 {
        3 + self.skydome.is_some() as u8
    }

    unsafe fn submit_begin(&mut self, viewport: &Viewport) {
        trace!("submit_begin");

        let mut dst = self.dst.borrow_mut();
        let pool = self.pool.as_mut().unwrap();
        let (frame_buf, render_pass_mode) = self.frame_buf.as_ref().unwrap();
        let render_pass = pool.render_pass(&self.driver, *render_pass_mode);
        let mut color_metal = self.geom_buf.color_metal.borrow_mut();
        let mut normal_rough = self.geom_buf.normal_rough.borrow_mut();
        let mut light = self.geom_buf.light.borrow_mut();
        let mut output = self.geom_buf.output.borrow_mut();
        let mut depth = self.geom_buf.depth.borrow_mut();
        let dims = dst.dims();
        let depth_clear = ClearValue {
            depth_stencil: ClearDepthStencil {
                depth: 1.0,
                stencil: 0,
            },
        };
        let light_clear = ClearValue {
            color: ClearColor {
                float32: [0.0, 0.0, 0.0, 0.0],
            }, // f32::NAN?
        };

        // Begin
        self.cmd_buf
            .begin_primary(CommandBufferFlags::ONE_TIME_SUBMIT);

        // Optional Step 1: Copy dst into the color render target
        if self.dst_preserve {
            dst.set_layout(
                &mut self.cmd_buf,
                Layout::TransferSrcOptimal,
                PipelineStage::TRANSFER,
                ImageAccess::TRANSFER_READ,
            );
            color_metal.set_layout(
                &mut self.cmd_buf,
                Layout::TransferDstOptimal,
                PipelineStage::TRANSFER,
                ImageAccess::TRANSFER_WRITE,
            );
            self.cmd_buf.copy_image(
                dst.as_ref(),
                Layout::TransferSrcOptimal,
                color_metal.as_ref(),
                Layout::TransferDstOptimal,
                once(ImageCopy {
                    src_subresource: SubresourceLayers {
                        aspects: Aspects::COLOR,
                        level: 0,
                        layers: 0..1,
                    },
                    src_offset: Offset::ZERO,
                    dst_subresource: SubresourceLayers {
                        aspects: Aspects::COLOR,
                        level: 0,
                        layers: 0..1,
                    },
                    dst_offset: Offset::ZERO,
                    extent: dims.as_extent_depth(1),
                }),
            );
        }

        // Prepare the render pass for mesh rendering
        color_metal.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        normal_rough.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        light.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        output.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        depth.set_layout(
            &mut self.cmd_buf,
            Layout::DepthStencilAttachmentOptimal,
            PipelineStage::LATE_FRAGMENT_TESTS, // TODO: VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT or VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT
            ImageAccess::DEPTH_STENCIL_ATTACHMENT_WRITE,
        );
        self.cmd_buf.begin_render_pass(
            render_pass,
            frame_buf.as_ref(),
            viewport.rect,
            &[depth_clear, light_clear],
            SubpassContents::Inline,
        );
    }

    unsafe fn submit_data_transfer(&mut self, instr: DataTransferInstruction) {
        trace!("submit_data_transfer");

        instr.src.transfer_range(
            &mut self.cmd_buf,
            instr.dst,
            CopyRange {
                src: instr.src_range,
                dst: 0,
            },
        );
    }

    unsafe fn submit_index_write_ref(&mut self, mut instr: DataWriteRefInstruction) {
        trace!("submit_index_write_ref");

        instr.buf.write_range(
            &mut self.cmd_buf,
            PipelineStage::VERTEX_INPUT, // TODO: Should be DRAW_INDIRECT?
            BufferAccess::INDEX_BUFFER_READ,
            instr.range,
        );
    }

    unsafe fn submit_lines(
        &mut self,
        instr: LineDrawInstruction,
        viewport: &Viewport,
        transform: Mat4,
    ) {
        trace!("submit_lines");

        let subpass_idx = self.post_fx_subpass_idx();
        let pool = self.pool.as_mut().unwrap();
        let (_, render_pass_mode) = self.frame_buf.as_ref().unwrap();

        // Lazy-init point light graphics
        assert!(self.graphics_line.is_none());
        self.graphics_line = Some(pool.graphics(
            #[cfg(feature = "debug-names")]
            &format!("{} line", &self.name),
            &self.driver,
            *render_pass_mode,
            subpass_idx,
            GraphicsMode::DrawLine,
        ));
        let graphics = self.graphics_line.as_ref().unwrap();

        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.push_graphics_constants(
            graphics.layout(),
            ShaderStageFlags::VERTEX,
            0,
            Mat4PushConst { val: transform }.as_ref(),
        );
        self.cmd_buf.bind_vertex_buffers(
            0,
            Some((
                instr.buf.as_ref(),
                SubRange {
                    offset: 0,
                    size: Some((instr.line_count * LINE_STRIDE as u32) as _),
                },
            )),
        );
        self.cmd_buf.draw(0..instr.line_count, 0..1);
    }

    unsafe fn submit_light_begin(&mut self) {
        trace!("submit_light_begin");

        self.cmd_buf.next_subpass(SubpassContents::Inline);
    }

    unsafe fn submit_light_bind(&mut self, instr: LightBindInstruction) {
        trace!("submit_light_bind");

        self.cmd_buf.bind_vertex_buffers(
            0,
            once((
                instr.buf.as_ref(),
                SubRange {
                    offset: 0,
                    size: Some(instr.buf_len),
                },
            )),
        );
    }

    unsafe fn submit_mesh_begin(&mut self, viewport: &Viewport) {
        trace!("submit_mesh_begin");

        let graphics = self.graphics_mesh.as_ref().unwrap();

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
    }

    unsafe fn submit_mesh_bind(&mut self, instr: MeshBindInstruction<'_>) {
        trace!("submit_mesh_bind");

        // NOTE: These sub ranges are not SubRange::WHOLE because the leased data may have
        // additional capacity beyond the indices/vertices we're using

        self.cmd_buf.bind_index_buffer(IndexBufferView {
            buffer: instr.idx_buf.as_ref(),
            index_type: instr.idx_ty.into(),
            range: SubRange {
                offset: 0,
                size: Some(instr.idx_buf_len),
            },
        });
        self.cmd_buf.bind_vertex_buffers(
            0,
            once((
                instr.vertex_buf.as_ref(),
                SubRange {
                    offset: 0,
                    size: Some(instr.vertex_buf_len),
                },
            )),
        );
    }

    unsafe fn submit_mesh_descriptors(&mut self, desc_set: usize) {
        trace!("submit_mesh_descriptors");

        let graphics = self.graphics_mesh.as_ref().unwrap();
        let desc_set = graphics.desc_set(desc_set);
        let layout = graphics.layout();

        bind_graphics_descriptor_set(&mut self.cmd_buf, layout, desc_set);
    }

    unsafe fn submit_mesh(&mut self, instr: MeshDrawInstruction, view_proj: Mat4) {
        trace!("submit_mesh");

        let graphics = self.graphics_mesh.as_ref().unwrap();
        let layout = graphics.layout();
        let world_view_proj = view_proj * instr.transform;

        for mesh in instr.meshes.filter(|mesh| !mesh.is_animated()) {
            let world_view_proj = if let Some(transform) = mesh.transform() {
                world_view_proj * transform
            } else {
                world_view_proj
            };

            self.cmd_buf.push_graphics_constants(
                layout,
                ShaderStageFlags::VERTEX,
                0,
                Mat4PushConst {
                    val: world_view_proj,
                }
                .as_ref(),
            );
            self.cmd_buf
                .draw_indexed(mesh.indices(), mesh.base_vertex() as _, 0..1);
        }
    }

    unsafe fn submit_point_lights(
        &mut self,
        instr: PointLightDrawInstruction,
        viewport: &Viewport,
        view_proj: Mat4,
    ) {
        trace!("submit_point_lights");

        const POINT_LIGHT_DRAW_COUNT: u32 = POINT_LIGHT.len() as u32 / 12;

        let subpass_idx = self.accum_light_subpass_idx();
        let pool = self.pool.as_mut().unwrap();
        let (_, render_pass_mode) = self.frame_buf.as_ref().unwrap();

        // Lazy-init point light graphics
        assert!(self.graphics_point_light.is_none());
        self.graphics_point_light = Some(pool.graphics(
            #[cfg(feature = "debug-names")]
            &self.name,
            &self.driver,
            *render_pass_mode,
            subpass_idx,
            GraphicsMode::DrawPointLight,
        ));
        let graphics = self.graphics_point_light.as_ref().unwrap();

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]); // TODO: Not sure this is needed!
        self.cmd_buf.set_viewports(0, &[viewport.clone()]); // TODO: Not sure this is needed!
        self.cmd_buf.bind_vertex_buffers(
            0,
            once((
                instr.buf.as_ref(),
                SubRange {
                    offset: 0,
                    size: Some(POINT_LIGHT.len() as _),
                },
            )),
        );

        for light in instr.lights {
            let world_view_proj = view_proj * Mat4::from_translation(light.center);

            self.cmd_buf.push_graphics_constants(
                graphics.layout(),
                ShaderStageFlags::VERTEX,
                0,
                Mat4PushConst {
                    val: world_view_proj,
                }
                .as_ref(),
            );
            self.cmd_buf.push_graphics_constants(
                graphics.layout(),
                ShaderStageFlags::VERTEX,
                Mat4PushConst::BYTE_LEN,
                PointLightPushConsts {
                    intensity: light.color.to_rgb() * light.lumens,
                    radius: light.radius,
                }
                .as_ref(),
            );
            self.cmd_buf.draw(0..POINT_LIGHT_DRAW_COUNT, 0..1);
        }
    }

    unsafe fn submit_rect_light_begin(&mut self, viewport: &Viewport) {
        trace!("submit_rect_light_begin");

        let subpass_idx = self.accum_light_subpass_idx();
        let pool = self.pool.as_mut().unwrap();
        let (_, render_pass_mode) = self.frame_buf.as_ref().unwrap();

        // Lazy-init rect light graphics
        assert!(self.graphics_rect_light.is_none());
        self.graphics_rect_light = Some(pool.graphics(
            #[cfg(feature = "debug-names")]
            &self.name,
            &self.driver,
            *render_pass_mode,
            subpass_idx,
            GraphicsMode::DrawRectLight,
        ));
        let graphics = self.graphics_rect_light.as_ref().unwrap();

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
    }

    unsafe fn submit_rect_light(&mut self, instr: RectLightDrawInstruction, view_proj: Mat4) {
        trace!("submit_rect_light");

        const RECT_LIGHT_DRAW_COUNT: u32 = RECT_LIGHT_STRIDE as u32 / 12;

        let graphics = self.graphics_rect_light.as_ref().unwrap();

        self.cmd_buf.push_graphics_constants(
            graphics.layout(),
            ShaderStageFlags::FRAGMENT,
            0,
            RectLightPushConsts {
                dims: instr.light.dims.into(),
                intensity: instr.light.color.to_rgb() * instr.light.lumens,
                normal: instr.light.normal,
                position: instr.light.position,
                radius: instr.light.radius,
                range: instr.light.range,
                view_proj,
            }
            .as_ref(),
        );
        self.cmd_buf
            .draw(instr.offset..instr.offset + RECT_LIGHT_DRAW_COUNT, 0..1);
    }

    unsafe fn submit_skydome(&mut self, viewport: &Viewport, view: Mat4) {
        trace!("submit_skydome");

        let graphics = self.graphics_skydome.as_ref().unwrap();
        let desc_set = graphics.desc_set(0);
        let layout = graphics.layout();
        let (skydome, buf, buf_len, _) = self.skydome.as_ref().unwrap();
        let vertex_count = *buf_len as u32 / 12;
        let star_rotation = Mat3::from_quat(skydome.star_rotation).to_cols_array_2d();

        let mut vertex_push_consts = SkydomeVertexPushConsts::default();
        vertex_push_consts.star_rotation_col0 = star_rotation[0].into();
        vertex_push_consts.star_rotation_col1 = star_rotation[1].into();
        vertex_push_consts.star_rotation_col2 = star_rotation[2].into();
        vertex_push_consts.view = view.inverse();

        let mut frag_push_consts = SkydomeFragmentPushConsts::default();
        frag_push_consts.sun_normal = skydome.sun_normal;
        frag_push_consts.time = skydome.time;
        frag_push_consts.weather = skydome.weather;

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
        self.cmd_buf.bind_vertex_buffers(
            0,
            once((
                buf.as_ref(),
                SubRange {
                    offset: 0,
                    size: Some(*buf_len),
                },
            )),
        );
        self.cmd_buf.push_graphics_constants(
            layout,
            ShaderStageFlags::VERTEX,
            0,
            vertex_push_consts.as_ref(),
        );
        self.cmd_buf.push_graphics_constants(
            layout,
            ShaderStageFlags::FRAGMENT,
            SkydomeVertexPushConsts::BYTE_LEN,
            frag_push_consts.as_ref(),
        );
        bind_graphics_descriptor_set(&mut self.cmd_buf, layout, desc_set);
        self.cmd_buf.draw(0..vertex_count, 0..1);
        self.cmd_buf.next_subpass(SubpassContents::Inline);
    }

    unsafe fn submit_skydome_write(&mut self) {
        trace!("submit_skydome_write");

        let (_, buf, len, _) = self.skydome.as_mut().unwrap();

        buf.write_range(
            &mut self.cmd_buf,
            PipelineStage::VERTEX_INPUT,
            BufferAccess::VERTEX_BUFFER_READ,
            0..*len,
        );
    }

    unsafe fn submit_spotlight_begin(&mut self, viewport: &Viewport) {
        trace!("submit_spotlight_begin");

        let subpass_idx = self.accum_light_subpass_idx();
        let pool = self.pool.as_mut().unwrap();
        let (_, render_pass_mode) = self.frame_buf.as_ref().unwrap();

        // Lazy-init spotlight graphics
        assert!(self.graphics_spotlight.is_none());
        self.graphics_spotlight = Some(pool.graphics(
            #[cfg(feature = "debug-names")]
            &self.name,
            &self.driver,
            *render_pass_mode,
            subpass_idx,
            GraphicsMode::DrawSpotlight,
        ));
        let graphics = self.graphics_spotlight.as_ref().unwrap();

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
    }

    unsafe fn submit_spotlight(&mut self, instr: SpotlightDrawInstruction, view_proj: Mat4) {
        trace!("submit_spotlight");

        const SPOTLIGHT_DRAW_COUNT: u32 = SPOTLIGHT_STRIDE as u32 / 12;

        let graphics = self.graphics_spotlight.as_ref().unwrap();

        /*let up = Vec3::unit_z();
        let light_view = Mat4::look_at_rh(e.position, e.position + e.normal, up);
        let light_space =
            Mat4::perspective_rh_gl(2.0 * e.cutoff_outer, 1.0, 1.0, 35.0) * light_view;
        let cutoff_inner = e.cutoff_inner.cos();
        let cutoff_outer = e.cutoff_outer.cos();
        draw_commands.push(
            SpotlightCommand {
                anormal: -e.normal,
                cutoff_inner,
                cutoff_outer,
                diffuse: e.diffuse,
                position: e.position,
                power: e.power,
                light_space,
            }
            .into(),
        );*/

        self.cmd_buf.push_graphics_constants(
            graphics.layout(),
            ShaderStageFlags::FRAGMENT,
            0,
            Mat4PushConst { val: view_proj }.as_ref(),
        );

        self.cmd_buf
            .draw(instr.offset..instr.offset + SPOTLIGHT_DRAW_COUNT, 0..1);
    }

    unsafe fn submit_sunlights<'c, L: Iterator<Item = &'c SunlightCommand>>(
        &mut self,
        lights: L,
        viewport: &Viewport,
    ) {
        trace!("submit_sunlights");

        let subpass_idx = self.accum_light_subpass_idx();
        let pool = self.pool.as_mut().unwrap();
        let (_, render_pass_mode) = self.frame_buf.as_ref().unwrap();

        // Lazy-init sunlight graphics
        assert!(self.graphics_sunlight.is_none());
        self.graphics_sunlight = Some(pool.graphics(
            #[cfg(feature = "debug-names")]
            &self.name,
            &self.driver,
            *render_pass_mode,
            subpass_idx,
            GraphicsMode::DrawSunlight,
        ));
        let graphics = self.graphics_sunlight.as_ref().unwrap();

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
        /*let view_inv = camera.view_inv();

        // TODO: Calculate this with object AABBs once those are ready (any AABB inside both the camera and shadow projections)
        // Calculate the world-space coords of the eight points that make up our camera frustum
        // and calculate the min/max/mid coordinates of them
        let camera_world = [
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(-1.0, -1.0, -1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(-1.0, -1.0, 1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(-1.0, 1.0, -1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(-1.0, 1.0, 1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(1.0, -1.0, -1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(1.0, -1.0, 1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(1.0, 1.0, -1.0)), 1.0))
                .truncate(),
            (view_inv * vec4_from_vec3(camera.unproject_point(vec3(1.0, 1.0, 1.0)), 1.0))
                .truncate(),
        ];
        let (mut min_x, mut min_y, mut min_z, mut max_x, mut max_y, mut max_z) = {
            let p0 = camera_world[0];
            (p0.x(), p0.y(), p0.z(), p0.x(), p0.y(), p0.z())
        };
        for pi in &camera_world {
            min_x = pi.x().min(min_x);
            min_y = pi.y().min(min_y);
            min_z = pi.z().min(min_z);
            max_x = pi.x().max(max_x);
            max_y = pi.y().max(max_y);
            max_z = pi.z().max(max_z);
        }
        let mid_x = (max_x + min_x) / 2.0;
        let mid_y = (max_y + min_y) / 2.0;
        let mid_z = (max_z + min_z) / 2.0;
        let position = vec3(mid_x, mid_y, mid_z);
        let target = position + e.normal;
        let n_dot_x = e.normal.dot(Vec3::unit_x()).abs();
        let n_dot_y = e.normal.dot(Vec3::unit_y()).abs();
        let up = if n_dot_x < n_dot_y {
            Vec3::unit_x()
        } else {
            Vec3::unit_y()
        };
        let light_view = Mat4::look_at_rh(position, target, up);
        let light_world = [
            (light_view * vec4_from_vec3(camera_world[0], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[1], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[2], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[3], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[4], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[5], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[6], 1.0)).truncate(),
            (light_view * vec4_from_vec3(camera_world[7], 1.0)).truncate(),
        ];
        let (mut min_x, mut min_y, mut min_z, mut max_x, mut max_y, mut max_z) = {
            let p0 = light_world[0];
            (p0.x(), p0.y(), p0.z(), p0.x(), p0.y(), p0.z())
        };
        for pi in &light_world {
            min_x = pi.x().min(min_x);
            min_y = pi.y().min(min_y);
            min_z = pi.z().min(min_z);
            max_x = pi.x().max(max_x);
            max_y = pi.y().max(max_y);
            max_z = pi.z().max(max_z);
        }
        let light_space =
            Mat4::orthographic_rh(min_x, max_x, min_y, max_y, min_z, max_z) * light_view;

        Self {
            normal_inv: -e.normal,
            diffuse: e.diffuse,
            power: e.power,
            light_space,
        }*/

        for light in lights {
            self.cmd_buf.push_graphics_constants(
                graphics.layout(),
                ShaderStageFlags::FRAGMENT,
                0,
                SunlightPushConsts {
                    intensity: light.color.to_rgb() * light.lumens,
                    normal: light.normal,
                }
                .as_ref(),
            );

            self.cmd_buf.draw(0..6, 0..1);
        }
    }

    unsafe fn submit_vertex_attrs_begin(&mut self, instr: CalcVertexAttrsComputeMode) {
        trace!("submit_vertex_attrs_begin");

        let compute = match instr {
            CalcVertexAttrsComputeMode::U16 => self.compute_u16_vertex_attrs.as_ref(),
            CalcVertexAttrsComputeMode::U16_SKIN => self.compute_u16_skin_vertex_attrs.as_ref(),
            CalcVertexAttrsComputeMode::U32 => self.compute_u32_vertex_attrs.as_ref(),
            CalcVertexAttrsComputeMode::U32_SKIN => self.compute_u32_skin_vertex_attrs.as_ref(),
        }
        .unwrap();
        let pipeline = compute.pipeline();

        self.cmd_buf.bind_compute_pipeline(pipeline);
    }

    unsafe fn submit_vertex_attrs_descriptors(&mut self, instr: VertexAttrsDescriptorsInstruction) {
        trace!("submit_vertex_attrs_descriptors");

        let compute = match instr.mode {
            CalcVertexAttrsComputeMode::U16 => self.compute_u16_vertex_attrs.as_ref(),
            CalcVertexAttrsComputeMode::U16_SKIN => self.compute_u16_skin_vertex_attrs.as_ref(),
            CalcVertexAttrsComputeMode::U32 => self.compute_u32_vertex_attrs.as_ref(),
            CalcVertexAttrsComputeMode::U32_SKIN => self.compute_u32_skin_vertex_attrs.as_ref(),
        }
        .unwrap();
        let desc_set = compute.desc_set(instr.desc_set);
        let pool = self.pool.as_mut().unwrap();
        let (_, pipeline_layout) = pool.layouts.compute_calc_vertex_attrs(
            #[cfg(feature = "debug-names")]
            &self.name,
            &self.driver,
        );

        bind_compute_descriptor_set(&mut self.cmd_buf, pipeline_layout, desc_set);
    }

    unsafe fn submit_vertex_attrs_calc(&mut self, instr: DataComputeInstruction) {
        trace!("submit_vertex_attrs_calc");

        // TODO: Do I need to work within limits? Why is it not broken right now?
        let device = self.driver.borrow();
        let _limit = Device::gpu(&device).limits().max_compute_work_group_size[0];
        let pool = self.pool.as_mut().unwrap();
        let (_, pipeline_layout) = pool.layouts.compute_calc_vertex_attrs(
            #[cfg(feature = "debug-names")]
            &self.name,
            &self.driver,
        );

        // We may be limited by the count of dispatches we issue; so use a loop
        // to dispatch as many times as needed
        self.cmd_buf.push_compute_constants(
            pipeline_layout,
            0,
            CalcVertexAttrsPushConsts {
                base_idx: instr.base_idx,
                base_vertex: instr.base_vertex,
            }
            .as_ref(),
        );
        self.cmd_buf.dispatch([instr.dispatch, 1, 1]);
        // instr.buf.barrier_range(
        //     &mut self.cmd_buf,
        //     PipelineStage::COMPUTE_SHADER,
        //     BufferAccess::SHADER_READ,
        //     instr.range,
        // );
    }

    unsafe fn submit_vertex_copies(&mut self, instr: DataCopyInstruction) {
        trace!("submit_vertex_copies");

        instr.buf.copy_ranges(
            &mut self.cmd_buf,
            PipelineStage::VERTEX_INPUT,
            BufferAccess::VERTEX_BUFFER_READ,
            instr.ranges,
        );
    }

    unsafe fn submit_vertex_write(&mut self, instr: DataWriteInstruction) {
        trace!("submit_vertex_write");

        instr.buf.write_range(
            &mut self.cmd_buf,
            PipelineStage::VERTEX_INPUT,
            BufferAccess::VERTEX_BUFFER_READ,
            instr.range,
        );
    }

    unsafe fn submit_vertex_write_ref(&mut self, mut instr: DataWriteRefInstruction) {
        trace!("submit_vertex_write_ref");

        instr.buf.write_range(
            &mut self.cmd_buf,
            PipelineStage::COMPUTE_SHADER,
            BufferAccess::SHADER_READ,
            instr.range,
        );
    }

    unsafe fn submit_finish(&mut self) {
        trace!("submit_finish");

        let mut device = self.driver.borrow_mut();
        let mut dst = self.dst.borrow_mut();
        let mut output = self.geom_buf.output.borrow_mut();
        let dims = dst.dims();

        // Step 6: Copy the output graphics buffer into dst
        self.cmd_buf.end_render_pass();
        output.set_layout(
            &mut self.cmd_buf,
            Layout::TransferSrcOptimal,
            PipelineStage::TRANSFER,
            ImageAccess::TRANSFER_READ,
        );
        dst.set_layout(
            &mut self.cmd_buf,
            Layout::TransferDstOptimal,
            PipelineStage::TRANSFER,
            ImageAccess::TRANSFER_WRITE,
        );
        self.cmd_buf.copy_image(
            output.as_ref(),
            Layout::TransferSrcOptimal,
            dst.as_ref(),
            Layout::TransferDstOptimal,
            once(ImageCopy {
                src_subresource: SubresourceLayers {
                    aspects: Aspects::COLOR,
                    level: 0,
                    layers: 0..1,
                },
                src_offset: Offset::ZERO,
                dst_subresource: SubresourceLayers {
                    aspects: Aspects::COLOR,
                    level: 0,
                    layers: 0..1,
                },
                dst_offset: Offset::ZERO,
                extent: dims.as_extent_depth(1),
            }),
        );

        // Finish
        self.cmd_buf.finish();

        // Submit
        Device::queue_mut(&mut device).submit(
            Submission {
                command_buffers: once(&self.cmd_buf),
                wait_semaphores: empty(),
                signal_semaphores: empty::<&<_Backend as Backend>::Semaphore>(),
            },
            Some(self.fence.as_ref()),
        );
    }

    unsafe fn write_material_descriptors<'m>(
        device: &Device,
        graphics: &Graphics,
        materials: impl ExactSizeIterator<Item = &'m Material>,
    ) {
        for (idx, material) in materials.enumerate() {
            let set = graphics.desc_set(idx);
            device.write_descriptor_sets(vec![
                DescriptorSetWrite {
                    set,
                    binding: 0,
                    array_offset: 0,
                    descriptors: once(Descriptor::CombinedImageSampler(
                        material.color.borrow().as_default_view().as_ref(),
                        Layout::ShaderReadOnlyOptimal,
                        graphics.sampler(0).as_ref(),
                    )),
                },
                DescriptorSetWrite {
                    set,
                    binding: 1,
                    array_offset: 0,
                    descriptors: once(Descriptor::CombinedImageSampler(
                        material.metal_rough.borrow().as_default_view().as_ref(),
                        Layout::ShaderReadOnlyOptimal,
                        graphics.sampler(1).as_ref(),
                    )),
                },
                DescriptorSetWrite {
                    set,
                    binding: 2,
                    array_offset: 0,
                    descriptors: once(Descriptor::CombinedImageSampler(
                        material.normal.borrow().as_default_view().as_ref(),
                        Layout::ShaderReadOnlyOptimal,
                        graphics.sampler(2).as_ref(),
                    )),
                },
            ]);
        }
    }

    unsafe fn write_skydome_descriptors(device: &Device, graphics: &Graphics, skydome: &Skydome) {
        let set = graphics.desc_set(0);
        device.write_descriptor_sets(vec![
            DescriptorSetWrite {
                set,
                binding: 0,
                array_offset: 0,
                descriptors: once(Descriptor::CombinedImageSampler(
                    skydome.cloud[0].borrow().as_default_view().as_ref(),
                    Layout::ShaderReadOnlyOptimal,
                    graphics.sampler(0).as_ref(),
                )),
            },
            DescriptorSetWrite {
                set,
                binding: 1,
                array_offset: 0,
                descriptors: once(Descriptor::CombinedImageSampler(
                    skydome.cloud[1].borrow().as_default_view().as_ref(),
                    Layout::ShaderReadOnlyOptimal,
                    graphics.sampler(1).as_ref(),
                )),
            },
            DescriptorSetWrite {
                set,
                binding: 2,
                array_offset: 0,
                descriptors: once(Descriptor::CombinedImageSampler(
                    skydome.moon.borrow().as_default_view().as_ref(),
                    Layout::ShaderReadOnlyOptimal,
                    graphics.sampler(2).as_ref(),
                )),
            },
            DescriptorSetWrite {
                set,
                binding: 3,
                array_offset: 0,
                descriptors: once(Descriptor::CombinedImageSampler(
                    skydome.sun.borrow().as_default_view().as_ref(),
                    Layout::ShaderReadOnlyOptimal,
                    graphics.sampler(3).as_ref(),
                )),
            },
            DescriptorSetWrite {
                set,
                binding: 4,
                array_offset: 0,
                descriptors: once(Descriptor::CombinedImageSampler(
                    skydome.tint[0].borrow().as_default_view().as_ref(),
                    Layout::ShaderReadOnlyOptimal,
                    graphics.sampler(4).as_ref(),
                )),
            },
            DescriptorSetWrite {
                set,
                binding: 5,
                array_offset: 0,
                descriptors: once(Descriptor::CombinedImageSampler(
                    skydome.tint[1].borrow().as_default_view().as_ref(),
                    Layout::ShaderReadOnlyOptimal,
                    graphics.sampler(5).as_ref(),
                )),
            },
        ]);
    }

    unsafe fn write_vertex_descriptors<'v>(
        device: &Device,
        compute: &Compute,
        vertex_bufs: impl ExactSizeIterator<Item = VertexBuffers<'v>>,
    ) {
        for (idx, vertex_buf) in vertex_bufs.enumerate() {
            let set = compute.desc_set(idx);
            device.write_descriptor_sets(vec![
                DescriptorSetWrite {
                    set,
                    binding: 0,
                    array_offset: 0,
                    descriptors: once(Descriptor::Buffer(
                        vertex_buf.idx.as_ref(),
                        SubRange {
                            offset: 0,
                            size: Some(vertex_buf.idx_len),
                        },
                    )),
                },
                DescriptorSetWrite {
                    set,
                    binding: 1,
                    array_offset: 0,
                    descriptors: once(Descriptor::Buffer(
                        vertex_buf.src.as_ref(),
                        SubRange {
                            offset: 0,
                            size: Some(vertex_buf.src_len),
                        },
                    )),
                },
                DescriptorSetWrite {
                    set,
                    binding: 2,
                    array_offset: 0,
                    descriptors: once(Descriptor::Buffer(
                        vertex_buf.dst.as_ref(),
                        SubRange {
                            offset: 0,
                            size: Some(vertex_buf.dst_len),
                        },
                    )),
                },
                DescriptorSetWrite {
                    set,
                    binding: 3,
                    array_offset: 0,
                    descriptors: once(Descriptor::Buffer(
                        vertex_buf.write_mask.as_ref(),
                        SubRange {
                            offset: 0,
                            size: Some(vertex_buf.write_mask_len),
                        },
                    )),
                },
            ]);
        }
    }
}

impl Drop for DrawOp {
    fn drop(&mut self) {
        self.wait();

        // Causes the compiler to drop internal caches which store texture refs; they were being held
        // alive there so that they could not be dropped until we finished GPU execution
        if let Some(compiler) = self.compiler.as_mut() {
            compiler.reset();
        }
    }
}

impl Op for DrawOp {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn take_pool(&mut self) -> Option<Lease<Pool>> {
        self.pool.take()
    }

    fn wait(&self) {
        Fence::wait(&self.fence);
    }
}

struct LineInstruction(u32);

#[derive(Clone, Debug)]
pub struct LineCommand([LineVertex; 2]);

#[derive(Clone, Debug)]
struct LineVertex {
    color: AlphaColor,
    pos: Vec3,
}

#[derive(Clone, Debug)]
pub struct Material {
    pub color: BitmapRef,
    pub metal_rough: BitmapRef,
    pub normal: BitmapRef,
}

impl Eq for Material {}

impl Hash for Material {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.color.as_ptr().hash(state);
        self.metal_rough.as_ptr().hash(state);
        self.normal.as_ptr().hash(state);
    }
}

impl Ord for Material {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut res = BitmapRef::as_ptr(&self.color).cmp(&BitmapRef::as_ptr(&other.color));
        if res != Ordering::Less {
            return res;
        }

        res = BitmapRef::as_ptr(&self.metal_rough).cmp(&BitmapRef::as_ptr(&other.metal_rough));
        if res != Ordering::Less {
            return res;
        }

        BitmapRef::as_ptr(&self.normal).cmp(&BitmapRef::as_ptr(&other.normal))
    }
}

impl PartialEq for Material {
    fn eq(&self, other: &Self) -> bool {
        BitmapRef::ptr_eq(&self.color, &other.color)
            && BitmapRef::ptr_eq(&self.normal, &other.normal)
            && BitmapRef::ptr_eq(&self.metal_rough, &other.metal_rough)
    }
}

impl PartialOrd for Material {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug)]
pub struct Skydome {
    pub cloud: [BitmapRef; 2],
    pub moon: BitmapRef,
    pub sun: BitmapRef,
    pub sun_normal: Vec3,
    pub star_rotation: Quat,
    pub time: f32,
    pub tint: [BitmapRef; 2],
    pub weather: f32,
}
