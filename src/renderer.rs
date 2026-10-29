//! Shared, on-demand Metal renderer. All coordinates in this module are physical pixels.
//!
//! The terminal parser and text shaping run on the CPU; rectangles and cached glyphs are
//! composited on the GPU. Keeping one font system and atlas avoids duplicating them per pane.

use std::{collections::HashSet, mem, sync::Arc};

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use glyphon::cosmic_text::CacheKeyFlags;
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};
use winit::window::Window;

const CPU_GLYPH_CACHE_BYTES: usize = 8 * 1024 * 1024;
const CPU_GLYPH_CACHE_ENTRIES: usize = 4096;
const GLYPH_WORKING_SET: usize = 8192;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: [u8; 4],
}

#[derive(Clone, Debug)]
pub struct TextRun {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub font_size: f32,
    pub line_height: f32,
    pub color: [u8; 4],
    pub bold: bool,
    pub italic: bool,
    /// [left, top, right, bottom], in physical pixels.
    pub clip: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

struct CachedText {
    buffer: Buffer,
    text: String,
    font_size: f32,
    line_height: f32,
    bold: bool,
    italic: bool,
}

impl CachedText {
    fn new(fonts: &mut FontSystem, run: &TextRun) -> Self {
        let mut buffer = Buffer::new(fonts, Metrics::new(run.font_size, run.line_height));
        // The application already wraps terminal cells and clips other rows. Wrapping
        // again here would move terminal glyphs to the wrong screen coordinates.
        buffer.set_wrap(fonts, Wrap::None);
        buffer.set_size(fonts, None, Some(run.line_height));
        let mut cached = Self {
            buffer,
            text: String::new(),
            font_size: 0.0,
            line_height: 0.0,
            bold: !run.bold,
            italic: !run.italic,
        };
        cached.update(fonts, run);
        cached
    }

    fn update(&mut self, fonts: &mut FontSystem, run: &TextRun) {
        if self.text == run.text
            && self.font_size == run.font_size
            && self.line_height == run.line_height
            && self.bold == run.bold
            && self.italic == run.italic
        {
            return;
        }
        self.buffer.set_metrics_and_size(
            fonts,
            Metrics::new(run.font_size, run.line_height),
            None,
            Some(run.line_height),
        );
        let attrs = Attrs::new()
            .family(Family::Monospace)
            .weight(if run.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            })
            .style(if run.italic {
                Style::Italic
            } else {
                Style::Normal
            });
        self.buffer
            .set_text(fonts, &run.text, &attrs, Shaping::Advanced);
        self.buffer.shape_until_scroll(fonts, false);
        // Many CJK families have no italic face. cosmic-text restricts fallbacks to
        // the requested style, so retry missing italic glyphs with a synthetic slant.
        // The normal font's advance stays unchanged, preserving terminal columns.
        if run.italic
            && self
                .buffer
                .layout_runs()
                .any(|line| line.glyphs.iter().any(|glyph| glyph.glyph_id == 0))
        {
            self.buffer.set_text(
                fonts,
                &run.text,
                &attrs
                    .style(Style::Normal)
                    .cache_key_flags(CacheKeyFlags::FAKE_ITALIC),
                Shaping::Advanced,
            );
            self.buffer.shape_until_scroll(fonts, false);
        }
        self.text.clone_from(&run.text);
        self.font_size = run.font_size;
        self.line_height = run.line_height;
        self.bold = run.bold;
        self.italic = run.italic;
    }
}

pub struct Renderer {
    // The surface is dropped before the retained window.
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    rectangle_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    vertices: Vec<Vertex>,
    fonts: FontSystem,
    swash: SwashCache,
    cache: Cache,
    atlas: TextAtlas,
    viewport: Viewport,
    text_renderer: TextRenderer,
    text_buffers: Vec<CachedText>,
    metric_cache: Option<(f32, (f32, f32))>,
    glyph_working_set: HashSet<(char, u32, bool, bool)>,
    suspended: bool,
    pub adapter_name: String,
    window: Arc<Window>,
}

impl Renderer {
    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .context("creating the Metal window surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("finding a Metal GPU adapter")?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("LeafTerm shared GPU device"),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .await
            .context("creating the GPU device")?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(capabilities.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            // Avoid buffering several window-sized textures for a terminal that only
            // redraws when content changes. This is a latency hint, not a memory cap.
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("LeafTerm rectangle shader"),
            source: wgpu::ShaderSource::Wgsl(RECTANGLE_SHADER.into()),
        });
        let rectangle_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("LeafTerm rectangle pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                }],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let vertex_capacity = 1024;
        let vertex_buffer = make_vertex_buffer(&device, vertex_capacity);
        let mut fonts = FontSystem::new();
        fonts.db_mut().set_monospace_family("Menlo");
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            rectangle_pipeline,
            vertex_buffer,
            vertex_capacity,
            vertices: Vec::new(),
            fonts,
            swash: SwashCache::new(),
            cache,
            atlas,
            viewport,
            text_renderer,
            text_buffers: Vec::new(),
            metric_cache: None,
            glyph_working_set: HashSet::new(),
            suspended: size.width == 0 || size.height == 0,
            adapter_name,
            window,
        })
    }

    /// Measures the actual selected monospace font so PTY columns and painted cells agree.
    pub fn metrics(&mut self, font_size: f32) -> (f32, f32) {
        if let Some((cached_size, metrics)) = self.metric_cache
            && cached_size == font_size
        {
            return metrics;
        }
        let line_height = (font_size * 1.45).ceil();
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(font_size, line_height));
        buffer.set_text(
            &mut self.fonts,
            "M",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        let width = buffer
            .layout_runs()
            .next()
            .map(|run| run.line_w)
            .filter(|width| *width > 0.0)
            .unwrap_or(font_size * 0.6);
        let result = (width, line_height);
        self.metric_cache = Some((font_size, result));
        result
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.suspended = width == 0 || height == 0;
        if self.suspended {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn draw(&mut self, rects: &[Rect], texts: &[TextRun]) -> Result<()> {
        if self.suspended {
            return Ok(());
        }
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => {
                self.window.request_redraw();
                return Ok(());
            }
            Err(error) => return Err(error).context("acquiring a GPU frame"),
        };

        self.vertices.clear();
        for rect in rects {
            append_rectangle(
                &mut self.vertices,
                *rect,
                self.config.width as f32,
                self.config.height as f32,
            );
        }
        if self.vertices.len() > self.vertex_capacity
            || (self.vertex_capacity > 4096 && self.vertices.len() < self.vertex_capacity / 4)
        {
            self.vertex_capacity = self.vertices.len().max(1024).next_power_of_two();
            self.vertex_buffer = make_vertex_buffer(&self.device, self.vertex_capacity);
        }
        if !self.vertices.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.vertices));
        }
        if self.vertices.capacity() > self.vertices.len().max(4096) * 4 {
            self.vertices.shrink_to(self.vertices.len().max(4096));
        }

        // Only retain buffers for this frame's visible runs. A closed pane must not keep
        // its entire shaped screen alive in a renderer cache.
        for (index, run) in texts.iter().enumerate() {
            if let Some(buffer) = self.text_buffers.get_mut(index) {
                buffer.update(&mut self.fonts, run);
            } else {
                self.text_buffers
                    .push(CachedText::new(&mut self.fonts, run));
            }
            for character in run.text.chars() {
                self.glyph_working_set.insert((
                    character,
                    run.font_size.to_bits(),
                    run.bold,
                    run.italic,
                ));
            }
        }
        self.text_buffers.truncate(texts.len());
        if self.text_buffers.capacity() > texts.len().max(128) * 4 {
            self.text_buffers.shrink_to(texts.len().max(128));
        }
        // trim() enables LRU reuse but doesn't shrink atlas textures. Recreate after
        // substantial font/glyph churn (e.g. many zoom changes) to release high-water
        // allocations. This is a working-set policy, not a hard total GPU byte cap.
        if self.glyph_working_set.len() > GLYPH_WORKING_SET {
            self.atlas = TextAtlas::new(&self.device, &self.queue, &self.cache, self.config.format);
            self.text_renderer = TextRenderer::new(
                &mut self.atlas,
                &self.device,
                wgpu::MultisampleState::default(),
                None,
            );
            self.glyph_working_set.clear();
        }
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.fonts,
                &mut self.atlas,
                &self.viewport,
                self.text_buffers
                    .iter()
                    .zip(texts)
                    .map(|(cached, run)| TextArea {
                        buffer: &cached.buffer,
                        left: run.x,
                        top: run.y,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: run.clip[0].floor().max(0.0) as i32,
                            top: run.clip[1].floor().max(0.0) as i32,
                            right: run.clip[2].ceil().min(self.config.width as f32) as i32,
                            bottom: run.clip[3].ceil().min(self.config.height as f32) as i32,
                        },
                        default_color: Color::rgba(
                            run.color[0],
                            run.color[1],
                            run.color[2],
                            run.color[3],
                        ),
                        custom_glyphs: &[],
                    }),
                &mut self.swash,
            )
            .context("preparing GPU text")?;

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("LeafTerm frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("LeafTerm UI and terminal"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if !self.vertices.is_empty() {
                pass.set_pipeline(&self.rectangle_pipeline);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.draw(0..self.vertices.len() as u32, 0..1);
            }
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("rendering GPU text")?;
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();
        self.trim_cpu_glyphs();
        Ok(())
    }

    fn trim_cpu_glyphs(&mut self) {
        let bytes: usize = self
            .swash
            .image_cache
            .values()
            .filter_map(Option::as_ref)
            .map(|image| image.data.len())
            .sum();
        if bytes > CPU_GLYPH_CACHE_BYTES
            || self.swash.image_cache.len() > CPU_GLYPH_CACHE_ENTRIES
            || self.swash.outline_command_cache.len() > CPU_GLYPH_CACHE_ENTRIES
        {
            self.swash = SwashCache::new();
        }
    }
}

fn make_vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("LeafTerm reusable rectangle vertices"),
        size: (capacity * mem::size_of::<Vertex>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn append_rectangle(vertices: &mut Vec<Vertex>, rect: Rect, width: f32, height: f32) {
    let left = rect.x.max(0.0);
    let top = rect.y.max(0.0);
    let right = (rect.x + rect.width).min(width);
    let bottom = (rect.y + rect.height).min(height);
    if right <= left || bottom <= top || rect.color[3] == 0 {
        return;
    }
    let color = [
        linear_channel(rect.color[0]),
        linear_channel(rect.color[1]),
        linear_channel(rect.color[2]),
        f32::from(rect.color[3]) / 255.0,
    ];
    let point = |x, y| Vertex {
        position: [x / width * 2.0 - 1.0, 1.0 - y / height * 2.0],
        color,
    };
    vertices.extend([
        point(left, top),
        point(left, bottom),
        point(right, top),
        point(right, top),
        point(left, bottom),
        point(right, bottom),
    ]);
}

fn linear_channel(value: u8) -> f32 {
    let srgb = f32::from(value) / 255.0;
    if srgb <= 0.04045 {
        srgb / 12.92
    } else {
        ((srgb + 0.055) / 1.055).powf(2.4)
    }
}

const RECTANGLE_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex fn vertex_main(
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.color = color;
    return output;
}

@fragment fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
"#;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn chinese_remains_available_when_ansi_style_changes() {
        let mut fonts = FontSystem::new();
        fonts.db_mut().set_monospace_family("Menlo");
        let mut run = TextRun {
            x: 0.0,
            y: 0.0,
            text: "中文".into(),
            font_size: 26.0,
            line_height: 38.0,
            color: [255; 4],
            bold: false,
            italic: false,
            clip: [0.0, 0.0, 200.0, 38.0],
        };
        let mut cached = CachedText::new(&mut fonts, &run);
        for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
            run.bold = bold;
            run.italic = italic;
            cached.update(&mut fonts, &run);
            let glyphs: Vec<_> = cached
                .buffer
                .layout_runs()
                .flat_map(|line| line.glyphs)
                .collect();
            assert_eq!(glyphs.len(), 2);
            assert!(
                glyphs.iter().all(|glyph| glyph.glyph_id != 0),
                "missing Chinese glyph with bold={bold}, italic={italic}"
            );
        }
    }
}
