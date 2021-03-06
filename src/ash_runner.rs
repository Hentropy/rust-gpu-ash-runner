use ash::{
    extensions::{ext, khr},
    util::read_spv,
    version::{DeviceV1_0, EntryV1_0, InstanceV1_0},
    vk,
};

use winit::{
    event::{Event, VirtualKeyCode, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
};

use serde::Deserialize;

use std::{
    borrow::Cow,
    collections::HashMap,
    default::Default,
    ffi::{CStr, CString},
    fs::File,
    ops::Drop,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

use structopt::StructOpt;

#[derive(Copy, Clone)]
pub struct ShaderConstants {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, StructOpt)]
#[structopt()]
pub struct Options {
    /// Use Vulkan debug layer (requires Vulkan SDK installed)
    #[structopt(short, long)]
    debug_layer: bool,
}

// This is not an ideal solution, but it's simple and doesn't require an async runtime.
static NEEDS_REBUILD: AtomicBool = AtomicBool::new(false);
static IS_COMPILING: AtomicBool = AtomicBool::new(false);
static mut NEW_SHADERS: Vec<SpirvShader> = Vec::<SpirvShader>::new();

pub fn main() {
    let options = Options::from_args();
    let shaders = compile_shaders();

    // runtime setup
    let event_loop = EventLoop::<CompilerEvent>::with_user_event();
    let window = winit::window::WindowBuilder::new()
        .with_title("Rust GPU - ash")
        .with_inner_size(winit::dpi::LogicalSize::new(
            f64::from(1280),
            f64::from(720),
        ))
        .build(&event_loop)
        .unwrap();
    let mut ctx = RenderBase::new(window, &options).into_ctx();

    // Create shader module and pipelines
    for SpirvShader { name, spirv } in shaders {
        ctx.insert_shader_module(name, spirv);
    }
    ctx.build_pipelines(
        vk::PipelineCache::null(),
        vec![(
            VertexShaderEntryPoint {
                module: "sky_shader".into(),
                entry_point: "main_vs".into(),
            },
            FragmentShaderEntryPoint {
                module: "sky_shader".into(),
                entry_point: "main_fs".into(),
            },
        )],
    );

    event_loop.run(move |event, _window_target, control_flow| match event {
        Event::RedrawEventsCleared { .. } => {
            if !IS_COMPILING.load(Ordering::SeqCst) && NEEDS_REBUILD.load(Ordering::SeqCst) {
                // if a recompile isn't in progress, this is the only thread.
                unsafe {
                    for SpirvShader { name, spirv } in NEW_SHADERS.drain(..) {
                        ctx.insert_shader_module(name, spirv);
                    }
                }
                ctx.rebuild_pipelines(vk::PipelineCache::null());
                NEEDS_REBUILD.store(false, Ordering::SeqCst);
            }
            ctx.render();
        }
        Event::WindowEvent { event, .. } => match event {
            WindowEvent::KeyboardInput { input, .. } => match input.virtual_keycode {
                Some(VirtualKeyCode::Escape) => *control_flow = ControlFlow::Exit,
                Some(VirtualKeyCode::F5) => {
                    // cannot start multiple recompiles at once, cannot cancel either
                    if !IS_COMPILING.compare_and_swap(false, true, Ordering::SeqCst) {
                        std::thread::spawn(|| {
                            unsafe {
                                NEW_SHADERS = compile_shaders();
                            }
                            NEEDS_REBUILD.store(true, Ordering::SeqCst);
                            IS_COMPILING.store(false, Ordering::SeqCst);
                        });
                    }
                    *control_flow = ControlFlow::Wait;
                }
                _ => *control_flow = ControlFlow::Wait,
            },
            WindowEvent::Resized(_) => {
                ctx.recreate_swapchain();
            }
            WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
            _ => *control_flow = ControlFlow::Wait,
        },
        _ => *control_flow = ControlFlow::Wait,
    });
}

pub fn compile_shaders() -> Vec<SpirvShader> {
    // Check if/what needs rebuild
    // (cargo might just handle this on its own? ignore for now)

    let spirv_codegen_backend = String::from("codegen_backend=rustc_codegen_spirv.dll");
    let rustflags = format!("-Z {} -Z symbol-mangling-version=v0", spirv_codegen_backend);
    let manifest_path = "shaders\\Cargo.toml";
    let target_dir = "shaders\\target";

    // run a cargo process with spirv codegen
    let cargo_out = Command::new("cargo")
        .args(&["build", "--release"])
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(&["--target", "spirv-unknown-unknown"])
        .args(&["--message-format", "json-render-diagnostics"])
        .args(&["-Z", "build-std=core"])
        .env("RUSTFLAGS", rustflags)
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo failed to execute build");

    // parse the json output from cargo to get the artifact paths
    let spv_paths: Vec<PathBuf> = String::from_utf8(cargo_out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| match serde_json::from_str::<SpirvArtifacts>(line) {
            Ok(line) => Some(line),
            Err(_) => None,
        })
        .filter(|line| line.reason == "compiler-artifact")
        .last()
        .expect("No output artifacts")
        .filenames
        .expect("No artifact filenemaes")
        .into_iter()
        .filter(|filename| filename.ends_with(".spv"))
        .map(Into::into)
        .collect();

    // load the spirv data into memory
    let mut artifacts = Vec::<SpirvShader>::with_capacity(spv_paths.len());
    for path in spv_paths {
        let name = path.file_stem().unwrap().to_owned().into_string().unwrap();
        let mut file = File::open(path).unwrap();
        let spirv = read_spv(&mut file).unwrap();
        //let mut loader = rspirv::dr::Loader::new();
        //rspirv::binary::parse_words(&spirv, &mut loader).expect("Invalid spirv module");
        //let module = loader.module();
        artifacts.push(SpirvShader { name, spirv });
    }

    artifacts
}

#[derive(Deserialize)]
struct SpirvArtifacts {
    reason: String,
    filenames: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct SpirvShader {
    pub name: String,
    pub spirv: Vec<u32>,
}

#[non_exhaustive]
#[derive(Debug)]
pub enum CompilerEvent {
    Complete(Vec<SpirvShader>),
}

pub struct RenderBase {
    pub window: winit::window::Window,

    #[cfg(target_os = "macos")]
    pub entry: ash_molten::MoltenEntry,
    #[cfg(not(target_os = "macos"))]
    pub entry: ash::Entry,

    pub instance: ash::Instance,
    pub device: ash::Device,
    pub swapchain_loader: khr::Swapchain,

    pub debug_utils_loader: Option<ext::DebugUtils>,
    pub debug_call_back: Option<vk::DebugUtilsMessengerEXT>,

    pub pdevice: vk::PhysicalDevice,
    pub queue_family_index: u32,
    pub present_queue: vk::Queue,

    pub surface: vk::SurfaceKHR,
    pub surface_loader: khr::Surface,
    pub surface_format: vk::SurfaceFormatKHR,
}

impl RenderBase {
    pub fn new(window: winit::window::Window, options: &Options) -> Self {
        cfg_if::cfg_if! {
            if #[cfg(target_os = "macos")] {
                let entry = ash_molten::MoltenEntry::load().unwrap();
            } else {
                let entry = ash::Entry::new().unwrap();
            }
        }

        let instance: ash::Instance = {
            let app_name = CString::new("VulkanTriangle").unwrap();

            let layer_names = if options.debug_layer {
                vec![CString::new("VK_LAYER_KHRONOS_validation").unwrap()]
            } else {
                vec![]
            };
            let layers_names_raw: Vec<*const i8> = layer_names
                .iter()
                .map(|raw_name| raw_name.as_ptr())
                .collect();

            let mut extension_names_raw = ash_window::enumerate_required_extensions(&window)
                .unwrap()
                .iter()
                .map(|ext| ext.as_ptr())
                .collect::<Vec<_>>();
            if options.debug_layer {
                extension_names_raw.push(ext::DebugUtils::name().as_ptr());
            }

            let appinfo = vk::ApplicationInfo::builder()
                .application_name(&app_name)
                .application_version(0)
                .engine_name(&app_name)
                .engine_version(0)
                .api_version(vk::make_version(1, 1, 0));

            let instance_create_info = vk::InstanceCreateInfo::builder()
                .application_info(&appinfo)
                .enabled_layer_names(&layers_names_raw)
                .enabled_extension_names(&extension_names_raw);

            unsafe {
                entry
                    .create_instance(&instance_create_info, None)
                    .expect("Instance creation error")
            }
        };

        let surface =
            unsafe { ash_window::create_surface(&entry, &instance, &window, None).unwrap() };

        let (debug_utils_loader, debug_call_back) = if options.debug_layer {
            let debug_utils_loader = ext::DebugUtils::new(&entry, &instance);
            let debug_call_back = {
                let debug_info = vk::DebugUtilsMessengerCreateInfoEXT::builder()
                    .message_severity(
                        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                            | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                            | vk::DebugUtilsMessageSeverityFlagsEXT::INFO,
                    )
                    .message_type(vk::DebugUtilsMessageTypeFlagsEXT::all())
                    .pfn_user_callback(Some(vulkan_debug_callback));

                unsafe {
                    debug_utils_loader
                        .create_debug_utils_messenger(&debug_info, None)
                        .unwrap()
                }
            };

            (Some(debug_utils_loader), Some(debug_call_back))
        } else {
            (None, None)
        };

        let surface_loader = khr::Surface::new(&entry, &instance);

        let (pdevice, queue_family_index) = unsafe {
            instance
                .enumerate_physical_devices()
                .expect("Physical device error")
                .iter()
                .find_map(|pdevice| {
                    instance
                        .get_physical_device_queue_family_properties(*pdevice)
                        .iter()
                        .enumerate()
                        .find_map(|(index, ref info)| {
                            if info.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                                && surface_loader
                                    .get_physical_device_surface_support(
                                        *pdevice,
                                        index as u32,
                                        surface,
                                    )
                                    .unwrap()
                            {
                                Some((*pdevice, index as u32))
                            } else {
                                None
                            }
                        })
                })
                .expect("Couldn't find suitable device.")
        };

        let device: ash::Device = {
            let device_extension_names_raw = [khr::Swapchain::name().as_ptr()];
            let features = vk::PhysicalDeviceFeatures {
                shader_clip_distance: 1,
                ..Default::default()
            };
            let priorities = [1.0];
            let queue_info = [vk::DeviceQueueCreateInfo::builder()
                .queue_family_index(queue_family_index)
                .queue_priorities(&priorities)
                .build()];
            let device_create_info = vk::DeviceCreateInfo::builder()
                .queue_create_infos(&queue_info)
                .enabled_extension_names(&device_extension_names_raw)
                .enabled_features(&features);
            unsafe {
                instance
                    .create_device(pdevice, &device_create_info, None)
                    .unwrap()
            }
        };

        let swapchain_loader = khr::Swapchain::new(&instance, &device);

        let present_queue = unsafe { device.get_device_queue(queue_family_index as u32, 0) };

        let surface_format = {
            let acceptable_formats = {
                [
                    vk::Format::R8G8B8_SRGB,
                    vk::Format::B8G8R8_SRGB,
                    vk::Format::R8G8B8A8_SRGB,
                    vk::Format::B8G8R8A8_SRGB,
                    vk::Format::A8B8G8R8_SRGB_PACK32,
                ]
            };
            unsafe {
                *surface_loader
                    .get_physical_device_surface_formats(pdevice, surface)
                    .unwrap()
                    .iter()
                    .find(|sfmt| acceptable_formats.contains(&sfmt.format))
                    .expect("Unable to find suitable surface format.")
            }
        };

        RenderBase {
            entry,
            instance,
            device,
            queue_family_index,
            pdevice,
            window,
            surface_loader,
            surface_format,
            present_queue,
            swapchain_loader,
            surface,
            debug_call_back,
            debug_utils_loader,
        }
    }

    pub fn surface_resolution(&self) -> vk::Extent2D {
        let surface_capabilities = unsafe {
            self.surface_loader
                .get_physical_device_surface_capabilities(self.pdevice, self.surface)
                .unwrap()
        };
        match surface_capabilities.current_extent.width {
            std::u32::MAX => {
                let window_inner = self.window.inner_size();
                vk::Extent2D {
                    width: window_inner.width,
                    height: window_inner.height,
                }
            }
            _ => surface_capabilities.current_extent,
        }
    }

    pub fn into_ctx(self) -> RenderCtx {
        RenderCtx::from_base(self)
    }

    pub fn surface_capabilities(&self) -> vk::SurfaceCapabilitiesKHR {
        unsafe {
            self.surface_loader
                .get_physical_device_surface_capabilities(self.pdevice, self.surface)
                .unwrap()
        }
    }

    pub fn create_swapchain(&self) -> vk::SwapchainKHR {
        let surface_capabilities = self.surface_capabilities();
        let mut desired_image_count = surface_capabilities.min_image_count + 1;
        if surface_capabilities.max_image_count > 0
            && desired_image_count > surface_capabilities.max_image_count
        {
            desired_image_count = surface_capabilities.max_image_count;
        }
        let pre_transform = if surface_capabilities
            .supported_transforms
            .contains(vk::SurfaceTransformFlagsKHR::IDENTITY)
        {
            vk::SurfaceTransformFlagsKHR::IDENTITY
        } else {
            surface_capabilities.current_transform
        };
        let present_mode = unsafe {
            self.surface_loader
                .get_physical_device_surface_present_modes(self.pdevice, self.surface)
                .unwrap()
                .iter()
                .cloned()
                .find(|&mode| mode == vk::PresentModeKHR::MAILBOX)
                .unwrap_or(vk::PresentModeKHR::FIFO)
        };
        let swapchain_create_info = vk::SwapchainCreateInfoKHR::builder()
            .surface(self.surface)
            .min_image_count(desired_image_count)
            .image_color_space(self.surface_format.color_space)
            .image_format(self.surface_format.format)
            .image_extent(self.surface_resolution())
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(pre_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode)
            .clipped(true)
            .image_array_layers(1);
        unsafe {
            self.swapchain_loader
                .create_swapchain(&swapchain_create_info, None)
                .unwrap()
        }
    }

    pub fn create_image_views(&self, swapchain: vk::SwapchainKHR) -> Vec<vk::ImageView> {
        unsafe {
            self.swapchain_loader
                .get_swapchain_images(swapchain)
                .unwrap()
                .iter()
                .map(|&image| {
                    let create_view_info = vk::ImageViewCreateInfo::builder()
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.surface_format.format)
                        .components(vk::ComponentMapping {
                            r: vk::ComponentSwizzle::R,
                            g: vk::ComponentSwizzle::G,
                            b: vk::ComponentSwizzle::B,
                            a: vk::ComponentSwizzle::A,
                        })
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                        .image(image);
                    self.device
                        .create_image_view(&create_view_info, None)
                        .unwrap()
                })
                .collect()
        }
    }

    pub fn create_framebuffers(
        &self,
        image_views: &[vk::ImageView],
        render_pass: vk::RenderPass,
    ) -> Vec<vk::Framebuffer> {
        image_views
            .iter()
            .map(|&present_image_view| {
                let framebuffer_attachments = [present_image_view];
                let surface_resolution = self.surface_resolution();
                unsafe {
                    self.device
                        .create_framebuffer(
                            &vk::FramebufferCreateInfo::builder()
                                .render_pass(render_pass)
                                .attachments(&framebuffer_attachments)
                                .width(surface_resolution.width)
                                .height(surface_resolution.height)
                                .layers(1),
                            None,
                        )
                        .unwrap()
                }
            })
            .collect()
    }

    pub fn create_render_pass(&self) -> vk::RenderPass {
        let renderpass_attachments = [vk::AttachmentDescription {
            format: self.surface_format.format,
            samples: vk::SampleCountFlags::TYPE_1,
            load_op: vk::AttachmentLoadOp::CLEAR,
            store_op: vk::AttachmentStoreOp::STORE,
            final_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            ..Default::default()
        }];
        let color_attachment_refs = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let dependencies = [vk::SubpassDependency {
            src_subpass: vk::SUBPASS_EXTERNAL,
            src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_READ
                | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            ..Default::default()
        }];
        let subpasses = [vk::SubpassDescription::builder()
            .color_attachments(&color_attachment_refs)
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .build()];
        let renderpass_create_info = vk::RenderPassCreateInfo::builder()
            .attachments(&renderpass_attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);
        unsafe {
            self.device
                .create_render_pass(&renderpass_create_info, None)
                .unwrap()
        }
    }

    pub fn create_render_sync(&self) -> RenderSync {
        RenderSync::new(self)
    }

    pub fn create_render_command_pool(&self) -> RenderCommandPool {
        RenderCommandPool::new(self)
    }
}

impl Drop for RenderBase {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            if let Some((debug_utils, call_back)) =
                Option::zip(self.debug_utils_loader.take(), self.debug_call_back.take())
            {
                debug_utils.destroy_debug_utils_messenger(call_back, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

pub struct RenderSync {
    pub present_complete_semaphore: vk::Semaphore,
    pub rendering_complete_semaphore: vk::Semaphore,
    pub draw_commands_reuse_fence: vk::Fence,
    pub setup_commands_reuse_fence: vk::Fence,
}

impl RenderSync {
    pub fn new(base: &RenderBase) -> Self {
        let fence_create_info =
            vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::SIGNALED);

        let semaphore_create_info = vk::SemaphoreCreateInfo::default();

        unsafe {
            let draw_commands_reuse_fence = base
                .device
                .create_fence(&fence_create_info, None)
                .expect("Create fence failed.");
            let setup_commands_reuse_fence = base
                .device
                .create_fence(&fence_create_info, None)
                .expect("Create fence failed.");

            let present_complete_semaphore = base
                .device
                .create_semaphore(&semaphore_create_info, None)
                .unwrap();
            let rendering_complete_semaphore = base
                .device
                .create_semaphore(&semaphore_create_info, None)
                .unwrap();

            Self {
                present_complete_semaphore,
                rendering_complete_semaphore,
                draw_commands_reuse_fence,
                setup_commands_reuse_fence,
            }
        }
    }
}

pub struct RenderCommandPool {
    pub pool: vk::CommandPool,
    pub draw_command_buffer: vk::CommandBuffer,
    pub setup_command_buffer: vk::CommandBuffer,
}

impl RenderCommandPool {
    pub fn new(base: &RenderBase) -> Self {
        let pool = {
            let pool_create_info = vk::CommandPoolCreateInfo::builder()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(base.queue_family_index);

            unsafe {
                base.device
                    .create_command_pool(&pool_create_info, None)
                    .unwrap()
            }
        };

        let command_buffers = {
            let command_buffer_allocate_info = vk::CommandBufferAllocateInfo::builder()
                .command_buffer_count(2)
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY);

            unsafe {
                base.device
                    .allocate_command_buffers(&command_buffer_allocate_info)
                    .unwrap()
            }
        };

        let setup_command_buffer = command_buffers[0];
        let draw_command_buffer = command_buffers[1];

        Self {
            pool,
            draw_command_buffer,
            setup_command_buffer,
        }
    }
}

pub struct RenderCtx {
    pub base: RenderBase,
    pub sync: RenderSync,

    pub swapchain: vk::SwapchainKHR,
    pub image_views: Vec<vk::ImageView>,
    pub render_pass: vk::RenderPass,
    pub framebuffers: Vec<vk::Framebuffer>,
    pub commands: RenderCommandPool,
    pub viewports: Box<[vk::Viewport]>,
    pub scissors: Box<[vk::Rect2D]>,
    pub pipelines: Vec<Pipeline>,
    pub shader_modules: HashMap<String, vk::ShaderModule>,
    pub shader_set: Vec<(VertexShaderEntryPoint, FragmentShaderEntryPoint)>,

    pub compiler_thread: Option<bool>,
}

impl RenderCtx {
    pub fn from_base(base: RenderBase) -> Self {
        let sync = RenderSync::new(&base);

        let swapchain = base.create_swapchain();
        let image_views = base.create_image_views(swapchain);
        let render_pass = base.create_render_pass();
        let framebuffers = base.create_framebuffers(&image_views, render_pass);
        let commands = RenderCommandPool::new(&base);
        let (viewports, scissors) = {
            let surface_resolution = base.surface_resolution();
            (
                Box::new([vk::Viewport {
                    x: 0.0,
                    y: surface_resolution.height as f32,
                    width: surface_resolution.width as f32,
                    height: -(surface_resolution.height as f32),
                    min_depth: 0.0,
                    max_depth: 1.0,
                }]),
                Box::new([vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: surface_resolution,
                }]),
            )
        };

        RenderCtx {
            sync,
            base,
            swapchain,
            image_views,
            commands,
            render_pass,
            framebuffers,
            viewports,
            scissors,
            pipelines: Vec::new(),
            shader_modules: HashMap::new(),
            shader_set: Vec::new(),
            compiler_thread: None,
        }
    }

    pub fn create_pipeline_layout(&self) -> vk::PipelineLayout {
        let push_constant_range = vk::PushConstantRange::builder()
            .offset(0)
            .size(std::mem::size_of::<ShaderConstants>() as u32)
            .stage_flags(vk::ShaderStageFlags::all())
            .build();
        let layout_create_info = vk::PipelineLayoutCreateInfo::builder()
            .push_constant_ranges(&[push_constant_range])
            .build();
        unsafe {
            self.base
                .device
                .create_pipeline_layout(&layout_create_info, None)
                .unwrap()
        }
    }

    pub fn rebuild_pipelines(&mut self, pipeline_cache: vk::PipelineCache) {
        let pipeline_layout = self.create_pipeline_layout();
        let modules_names = self
            .shader_set
            .iter()
            .map(|(vert, frag)| {
                let vert_module = *self.shader_modules.get(&vert.module).unwrap();
                let vert_name = CString::new(vert.entry_point.clone()).unwrap();
                let frag_module = *self.shader_modules.get(&frag.module).unwrap();
                let frag_name = CString::new(frag.entry_point.clone()).unwrap();
                ((frag_module, frag_name), (vert_module, vert_name))
            })
            .collect::<Vec<_>>();
        let viewport = vk::PipelineViewportStateCreateInfo::builder();
        let descs = modules_names
            .iter()
            .map(|((frag_module, frag_name), (vert_module, vert_name))| {
                PipelineDescriptor::new(Box::new([
                    vk::PipelineShaderStageCreateInfo {
                        module: *vert_module,
                        p_name: (*vert_name).as_ptr(),
                        stage: vk::ShaderStageFlags::VERTEX,
                        ..Default::default()
                    },
                    vk::PipelineShaderStageCreateInfo {
                        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
                        module: *frag_module,
                        p_name: (*frag_name).as_ptr(),
                        stage: vk::ShaderStageFlags::FRAGMENT,
                        ..Default::default()
                    },
                ]))
            })
            .collect::<Vec<_>>();
        let pipeline_info = descs
            .iter()
            .map(|desc| {
                vk::GraphicsPipelineCreateInfo::builder()
                    .stages(&desc.shader_stages)
                    .vertex_input_state(&desc.vertex_input)
                    .input_assembly_state(&desc.input_assembly)
                    .rasterization_state(&desc.rasterization)
                    .multisample_state(&desc.multisample)
                    .depth_stencil_state(&desc.depth_stencil)
                    .color_blend_state(&desc.color_blend)
                    .dynamic_state(&desc.dynamic_state_info)
                    .viewport_state(&viewport)
                    .layout(pipeline_layout)
                    .render_pass(self.render_pass)
                    .build()
            })
            .collect::<Vec<_>>();
        self.pipelines = unsafe {
            self.base
                .device
                .create_graphics_pipelines(pipeline_cache, &pipeline_info, None)
                .expect("Unable to create graphics pipeline")
        }
        .iter()
        .zip(descs)
        .map(|(&pipeline, desc)| Pipeline {
            pipeline,
            pipeline_layout,
            color_blend_attachments: desc.color_blend_attachments,
            dynamic_state: desc.dynamic_state,
        })
        .collect();
    }

    pub fn build_pipelines(
        &mut self,
        pipeline_cache: vk::PipelineCache,
        shader_set: Vec<(VertexShaderEntryPoint, FragmentShaderEntryPoint)>,
    ) {
        self.shader_set = shader_set;
        self.rebuild_pipelines(pipeline_cache);
    }

    /// Add a shader module to the hash map of shader modules.  returns a handle to the module, and the
    /// old shader module if there was one with the same name already.  Does not rebuild pipelines
    /// that may be using the shader module, nor does it invalidate them.
    pub fn insert_shader_module(&mut self, name: String, spirv: Vec<u32>) {
        let shader_info = vk::ShaderModuleCreateInfo::builder().code(&spirv);
        let shader_module = unsafe {
            self.base
                .device
                .create_shader_module(&shader_info, None)
                .expect("Shader module error")
        };
        if let Some(old_module) = self.shader_modules.insert(name, shader_module) {
            unsafe { self.base.device.destroy_shader_module(old_module, None) }
        };
    }

    // Recreates the swapchain, but does not recreate the pipelines because they use dynamic state.
    pub fn recreate_swapchain(&mut self) {
        // cleanup
        unsafe {
            self.base.device.device_wait_idle().unwrap();
            // framebuffers
            for framebuffer in self.framebuffers.drain(..) {
                self.base.device.destroy_framebuffer(framebuffer, None)
            }
            // command buffers
            self.base.device.free_command_buffers(
                self.commands.pool,
                &[
                    self.commands.draw_command_buffer,
                    self.commands.setup_command_buffer,
                ],
            );
            // render pass
            self.base.device.destroy_render_pass(self.render_pass, None);
            // image views
            for image_view in self.image_views.drain(..) {
                self.base.device.destroy_image_view(image_view, None);
            }
            // swapchain
            self.base
                .swapchain_loader
                .destroy_swapchain(self.swapchain, None);
        }
        // swapchain
        self.swapchain = self.base.create_swapchain();
        // image_views
        self.image_views = self.base.create_image_views(self.swapchain);
        // render_pass
        self.render_pass = self.base.create_render_pass();
        // command buffers
        let command_buffers = {
            let command_buffer_allocate_info = vk::CommandBufferAllocateInfo::builder()
                .command_buffer_count(2)
                .command_pool(self.commands.pool)
                .level(vk::CommandBufferLevel::PRIMARY);

            unsafe {
                self.base
                    .device
                    .allocate_command_buffers(&command_buffer_allocate_info)
                    .unwrap()
            }
        };
        self.commands.setup_command_buffer = command_buffers[0];
        self.commands.draw_command_buffer = command_buffers[1];
        // framebuffers
        self.framebuffers = self
            .base
            .create_framebuffers(&self.image_views, self.render_pass);
    }

    pub fn render(&mut self) {
        let (present_index, _) = unsafe {
            self.base
                .swapchain_loader
                .acquire_next_image(
                    self.swapchain,
                    std::u64::MAX,
                    self.sync.present_complete_semaphore,
                    vk::Fence::null(),
                )
                .expect("failed to acquire next image")
        };

        let framebuffer = self.framebuffers[present_index as usize];
        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 1.0, 0.0],
            },
        }];

        for pipeline in self.pipelines.iter() {
            self.draw(pipeline, framebuffer, &clear_values);
        }

        let wait_semaphors = [self.sync.rendering_complete_semaphore];
        let swapchains = [self.swapchain];
        let image_indices = [present_index];
        let present_info = vk::PresentInfoKHR::builder()
            .wait_semaphores(&wait_semaphors)
            .swapchains(&swapchains)
            .image_indices(&image_indices);
        unsafe {
            self.base
                .swapchain_loader
                .queue_present(self.base.present_queue, &present_info)
                .expect("failed to present queue");
        }
    }

    pub fn draw(
        &self,
        pipeline: &Pipeline,
        framebuffer: vk::Framebuffer,
        clear_values: &[vk::ClearValue],
    ) {
        let render_pass_begin_info = vk::RenderPassBeginInfo::builder()
            .render_pass(self.render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.base.surface_resolution(),
            })
            .clear_values(clear_values)
            .build();
        self.record_submit_commandbuffer(
            &[vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT],
            |device, draw_command_buffer| {
                unsafe {
                    device.cmd_begin_render_pass(
                        draw_command_buffer,
                        &render_pass_begin_info,
                        vk::SubpassContents::INLINE,
                    );
                    device.cmd_bind_pipeline(
                        draw_command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline.pipeline,
                    );
                    device.cmd_set_viewport(draw_command_buffer, 0, &self.viewports);
                    device.cmd_set_scissor(draw_command_buffer, 0, &self.scissors);

                    let push_constants = ShaderConstants {
                        width: 1920, // ash runner currently does not support resizing.
                        height: 720,
                    };
                    device.cmd_push_constants(
                        draw_command_buffer,
                        pipeline.pipeline_layout,
                        ash::vk::ShaderStageFlags::all(),
                        0,
                        any_as_u8_slice(&push_constants),
                    );

                    device.cmd_draw(draw_command_buffer, 3, 1, 0, 0);
                    device.cmd_end_render_pass(draw_command_buffer);
                }
            },
        );
    }

    /// Helper function for submitting command buffers. Immediately waits for the fence before the command buffer
    /// is executed. That way we can delay the waiting for the fences by 1 frame which is good for performance.
    /// Make sure to create the fence in a signaled state on the first use.
    pub fn record_submit_commandbuffer<F: FnOnce(&ash::Device, vk::CommandBuffer)>(
        &self,
        wait_mask: &[vk::PipelineStageFlags],
        f: F,
    ) {
        unsafe {
            self.base
                .device
                .wait_for_fences(&[self.sync.draw_commands_reuse_fence], true, std::u64::MAX)
                .expect("Wait for fence failed.");

            self.base
                .device
                .reset_fences(&[self.sync.draw_commands_reuse_fence])
                .expect("Reset fences failed.");

            self.base
                .device
                .reset_command_buffer(
                    self.commands.draw_command_buffer,
                    vk::CommandBufferResetFlags::RELEASE_RESOURCES,
                )
                .expect("Reset command buffer failed.");

            let command_buffer_begin_info = vk::CommandBufferBeginInfo::builder()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);

            self.base
                .device
                .begin_command_buffer(
                    self.commands.draw_command_buffer,
                    &command_buffer_begin_info,
                )
                .expect("Begin commandbuffer");

            f(&self.base.device, self.commands.draw_command_buffer);

            self.base
                .device
                .end_command_buffer(self.commands.draw_command_buffer)
                .expect("End commandbuffer");

            let command_buffers = vec![self.commands.draw_command_buffer];
            let wait_semaphores = &[self.sync.present_complete_semaphore];
            let signal_semaphores = &[self.sync.rendering_complete_semaphore];
            let submit_info = vk::SubmitInfo::builder()
                .wait_semaphores(wait_semaphores)
                .wait_dst_stage_mask(wait_mask)
                .command_buffers(&command_buffers)
                .signal_semaphores(signal_semaphores);

            self.base
                .device
                .queue_submit(
                    self.base.present_queue,
                    &[submit_info.build()],
                    self.sync.draw_commands_reuse_fence,
                )
                .expect("queue submit failed.");
        }
    }
}

impl Drop for RenderCtx {
    fn drop(&mut self) {
        unsafe {
            self.base.device.device_wait_idle().unwrap();
            self.base
                .device
                .destroy_semaphore(self.sync.present_complete_semaphore, None);
            self.base
                .device
                .destroy_semaphore(self.sync.rendering_complete_semaphore, None);
            self.base
                .device
                .destroy_fence(self.sync.draw_commands_reuse_fence, None);
            self.base
                .device
                .destroy_fence(self.sync.setup_commands_reuse_fence, None);
            for &image_view in self.image_views.iter() {
                self.base.device.destroy_image_view(image_view, None);
            }
            self.base
                .device
                .destroy_command_pool(self.commands.pool, None);
            self.base
                .swapchain_loader
                .destroy_swapchain(self.swapchain, None);
        }
    }
}

pub struct VertexShaderEntryPoint {
    pub module: String,
    pub entry_point: String,
}

pub struct FragmentShaderEntryPoint {
    module: String,
    entry_point: String,
}

pub struct Pipeline {
    pub pipeline: vk::Pipeline,
    pub pipeline_layout: vk::PipelineLayout,
    pub color_blend_attachments: Box<[vk::PipelineColorBlendAttachmentState]>,
    pub dynamic_state: Box<[vk::DynamicState]>,
}

impl Pipeline {
    pub fn new(
        ctx: &RenderCtx,
        desc: PipelineDescriptor,
        pipeline_cache: vk::PipelineCache,
    ) -> Self {
        let viewport = vk::PipelineViewportStateCreateInfo::builder();
        let pipeline_layout = ctx.create_pipeline_layout();

        let pipeline_info = vk::GraphicsPipelineCreateInfo::builder()
            .stages(&desc.shader_stages)
            .vertex_input_state(&desc.vertex_input)
            .input_assembly_state(&desc.input_assembly)
            .rasterization_state(&desc.rasterization)
            .multisample_state(&desc.multisample)
            .depth_stencil_state(&desc.depth_stencil)
            .color_blend_state(&desc.color_blend)
            .dynamic_state(&desc.dynamic_state_info)
            .viewport_state(&viewport)
            .layout(pipeline_layout)
            .render_pass(ctx.render_pass);

        let pipeline = unsafe {
            ctx.base
                .device
                .create_graphics_pipelines(pipeline_cache, &[pipeline_info.build()], None)
                .expect("Unable to create graphics pipeline")
                .pop()
                .unwrap()
        };

        Self {
            pipeline_layout,
            pipeline,
            color_blend_attachments: desc.color_blend_attachments,
            dynamic_state: desc.dynamic_state,
        }
    }
}

pub struct PipelineDescriptor {
    pub color_blend_attachments: Box<[vk::PipelineColorBlendAttachmentState]>,
    pub dynamic_state: Box<[vk::DynamicState]>,
    pub shader_stages: Box<[vk::PipelineShaderStageCreateInfo]>,
    pub vertex_input: vk::PipelineVertexInputStateCreateInfo,
    pub input_assembly: vk::PipelineInputAssemblyStateCreateInfo,
    pub rasterization: vk::PipelineRasterizationStateCreateInfo,
    pub multisample: vk::PipelineMultisampleStateCreateInfo,
    pub depth_stencil: vk::PipelineDepthStencilStateCreateInfo,
    pub color_blend: vk::PipelineColorBlendStateCreateInfo,
    pub dynamic_state_info: vk::PipelineDynamicStateCreateInfo,
}

impl PipelineDescriptor {
    fn new(shader_stages: Box<[vk::PipelineShaderStageCreateInfo]>) -> Self {
        let vertex_input = vk::PipelineVertexInputStateCreateInfo {
            vertex_attribute_description_count: 0,
            vertex_binding_description_count: 0,
            ..Default::default()
        };
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo {
            topology: vk::PrimitiveTopology::TRIANGLE_LIST,
            ..Default::default()
        };

        let rasterization = vk::PipelineRasterizationStateCreateInfo {
            front_face: vk::FrontFace::COUNTER_CLOCKWISE,
            line_width: 1.0,
            polygon_mode: vk::PolygonMode::FILL,
            ..Default::default()
        };
        let multisample = vk::PipelineMultisampleStateCreateInfo {
            rasterization_samples: vk::SampleCountFlags::TYPE_1,
            ..Default::default()
        };
        let noop_stencil_state = vk::StencilOpState {
            fail_op: vk::StencilOp::KEEP,
            pass_op: vk::StencilOp::KEEP,
            depth_fail_op: vk::StencilOp::KEEP,
            compare_op: vk::CompareOp::ALWAYS,
            ..Default::default()
        };
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo {
            depth_test_enable: 0,
            depth_write_enable: 0,
            depth_compare_op: vk::CompareOp::ALWAYS,
            front: noop_stencil_state,
            back: noop_stencil_state,
            max_depth_bounds: 1.0,
            ..Default::default()
        };
        let color_blend_attachments = Box::new([vk::PipelineColorBlendAttachmentState {
            blend_enable: 0,
            src_color_blend_factor: vk::BlendFactor::SRC_COLOR,
            dst_color_blend_factor: vk::BlendFactor::ONE_MINUS_DST_COLOR,
            color_blend_op: vk::BlendOp::ADD,
            src_alpha_blend_factor: vk::BlendFactor::ZERO,
            dst_alpha_blend_factor: vk::BlendFactor::ZERO,
            alpha_blend_op: vk::BlendOp::ADD,
            color_write_mask: vk::ColorComponentFlags::all(),
        }]);
        let color_blend = vk::PipelineColorBlendStateCreateInfo::builder()
            .logic_op(vk::LogicOp::CLEAR)
            .attachments(color_blend_attachments.as_ref())
            .build();

        let dynamic_state = Box::new([vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR]);
        let dynamic_state_info = vk::PipelineDynamicStateCreateInfo::builder()
            .dynamic_states(dynamic_state.as_ref())
            .build();

        Self {
            shader_stages,
            vertex_input,
            input_assembly,
            rasterization,
            multisample,
            depth_stencil,
            color_blend_attachments,
            color_blend,
            dynamic_state,
            dynamic_state_info,
        }
    }
}

unsafe fn any_as_u8_slice<T: Sized>(p: &T) -> &[u8] {
    ::std::slice::from_raw_parts((p as *const T) as *const u8, ::std::mem::size_of::<T>())
}

unsafe extern "system" fn vulkan_debug_callback(
    message_severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    message_type: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _user_data: *mut std::os::raw::c_void,
) -> vk::Bool32 {
    let callback_data = *p_callback_data;
    let message_id_number: i32 = callback_data.message_id_number as i32;

    let message_id_name = if callback_data.p_message_id_name.is_null() {
        Cow::from("")
    } else {
        CStr::from_ptr(callback_data.p_message_id_name).to_string_lossy()
    };

    let message = if callback_data.p_message.is_null() {
        Cow::from("")
    } else {
        CStr::from_ptr(callback_data.p_message).to_string_lossy()
    };

    println!(
        "{:?}:\n{:?} [{} ({})] : {}\n",
        message_severity,
        message_type,
        message_id_name,
        &message_id_number.to_string(),
        message,
    );

    vk::FALSE
}
