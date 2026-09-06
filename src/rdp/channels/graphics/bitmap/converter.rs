//! Internal bitmap conversion for display-update fallback paths.
//!
//! Converts PipeWire frames into compact BGR bitmap rectangles for IronRDP
//! when the primary graphics path cannot use direct frame passthrough.

use std::{fmt, sync::Arc, time::Instant};

use crate::desktop::pipewire::{FfiDamageRegion, PixelFormat, VideoFrame, convert_format};
use parking_lot::RwLock;

const RDP_BITMAP_ALIGNMENT: usize = 64;
const BUFFER_POOL_SIZE: usize = 8;
const DAMAGE_THRESHOLD: f32 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RdpPixelFormat {
    BgrX32,
    Bgr24,
}

impl RdpPixelFormat {
    pub(crate) fn bytes_per_pixel(self) -> usize {
        match self {
            Self::BgrX32 => 4,
            Self::Bgr24 => 3,
        }
    }

    fn from_pixel_format(format: PixelFormat) -> Self {
        match format {
            PixelFormat::RGB | PixelFormat::BGR => Self::Bgr24,
            PixelFormat::BGRA
            | PixelFormat::BGRx
            | PixelFormat::RGBA
            | PixelFormat::RGBx
            | PixelFormat::NV12
            | PixelFormat::YUY2
            | PixelFormat::I420
            | PixelFormat::GRAY8 => Self::BgrX32,
        }
    }

    fn pipewire_format(self) -> PixelFormat {
        match self {
            Self::BgrX32 => PixelFormat::BGRx,
            Self::Bgr24 => PixelFormat::BGR,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rectangle {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rectangle {
    pub(crate) fn new(left: u16, top: u16, right: u16, bottom: u16) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    fn width(self) -> u16 {
        self.right.saturating_sub(self.left)
    }

    fn height(self) -> u16 {
        self.bottom.saturating_sub(self.top)
    }

    fn area(self) -> u32 {
        u32::from(self.width()) * u32::from(self.height())
    }

    fn intersects(self, other: Self) -> bool {
        !(self.right <= other.left
            || other.right <= self.left
            || self.bottom <= other.top
            || other.bottom <= self.top)
    }

    fn merge(&mut self, other: Self) {
        self.left = self.left.min(other.left);
        self.top = self.top.min(other.top);
        self.right = self.right.max(other.right);
        self.bottom = self.bottom.max(other.bottom);
    }
}

impl From<FfiDamageRegion> for Rectangle {
    fn from(damage: FfiDamageRegion) -> Self {
        Self {
            left: damage.x as u16,
            top: damage.y as u16,
            right: damage.x.saturating_add(damage.width as i32).max(0) as u16,
            bottom: damage.y.saturating_add(damage.height as i32).max(0) as u16,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BitmapData {
    pub rectangle: Rectangle,
    pub format: RdpPixelFormat,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct BitmapUpdate {
    pub rectangles: Vec<BitmapData>,
}

#[derive(Debug)]
pub(crate) enum ConversionError {
    InvalidFrame(String),
    ConversionFailed(String),
    RegionOutOfBounds,
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrame(message) => write!(f, "invalid frame: {message}"),
            Self::ConversionFailed(message) => write!(f, "conversion failed: {message}"),
            Self::RegionOutOfBounds => write!(f, "damage region is outside the frame"),
        }
    }
}

impl std::error::Error for ConversionError {}

#[derive(Clone)]
struct PooledBuffer {
    data: Vec<u8>,
    capacity: usize,
    last_used: Instant,
}

struct BufferPool {
    buffers: Vec<Option<PooledBuffer>>,
    free_indices: Vec<usize>,
}

impl BufferPool {
    fn new(size: usize) -> Self {
        Self {
            buffers: vec![None; size],
            free_indices: (0..size).collect(),
        }
    }

    fn acquire(&mut self, size: usize) -> Vec<u8> {
        for (idx, buffer) in self.buffers.iter_mut().enumerate() {
            if let Some(buffer) = buffer
                && buffer.capacity >= size
                && self.free_indices.contains(&idx)
            {
                self.free_indices.retain(|&i| i != idx);
                buffer.last_used = Instant::now();
                let mut data = std::mem::take(&mut buffer.data);
                data.resize(size, 0);
                return data;
            }
        }

        vec![0; align_to_boundary(size, RDP_BITMAP_ALIGNMENT)]
    }

    fn release(&mut self, mut buffer: Vec<u8>) {
        let capacity = buffer.capacity();
        buffer.clear();

        let idx = self.free_indices.pop().unwrap_or_else(|| {
            self.buffers
                .iter()
                .enumerate()
                .filter_map(|(idx, buffer)| buffer.as_ref().map(|buffer| (idx, buffer.last_used)))
                .min_by_key(|(_, last_used)| *last_used)
                .map_or(0, |(idx, _)| idx)
        });

        self.buffers[idx] = Some(PooledBuffer {
            data: buffer,
            capacity,
            last_used: Instant::now(),
        });
    }
}

struct DamageTracker {
    regions: Vec<Rectangle>,
    full_update: bool,
    screen_width: u16,
    screen_height: u16,
}

impl DamageTracker {
    fn new(width: u16, height: u16) -> Self {
        Self {
            regions: Vec::new(),
            full_update: false,
            screen_width: width,
            screen_height: height,
        }
    }

    fn add_damage(&mut self, region: Rectangle) {
        if self.full_update {
            return;
        }

        let total_area = u32::from(self.screen_width) * u32::from(self.screen_height);
        if total_area == 0 || region.area() as f32 / total_area as f32 > DAMAGE_THRESHOLD {
            self.full_update = true;
            self.regions.clear();
            return;
        }

        if let Some(existing) = self
            .regions
            .iter_mut()
            .find(|existing| existing.intersects(region))
        {
            existing.merge(region);
        } else {
            self.regions.push(region);
        }

        self.consolidate_regions();
    }

    fn consolidate_regions(&mut self) {
        let mut i = 0;
        while i < self.regions.len() {
            let mut j = i + 1;
            while j < self.regions.len() {
                if self.regions[i].intersects(self.regions[j]) {
                    let other = self.regions.remove(j);
                    self.regions[i].merge(other);
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    fn regions(&self) -> Vec<Rectangle> {
        if self.full_update {
            vec![Rectangle::new(0, 0, self.screen_width, self.screen_height)]
        } else {
            self.regions.clone()
        }
    }

    fn reset(&mut self) {
        self.regions.clear();
        self.full_update = false;
    }
}

#[derive(Debug, Clone, Default)]
struct ConversionStats {
    frames_converted: u64,
    bytes_processed: u64,
    conversion_time_ns: u64,
}

pub(crate) struct BitmapConverter {
    buffer_pool: Arc<RwLock<BufferPool>>,
    damage_tracker: Arc<RwLock<DamageTracker>>,
    last_frame_hash: u64,
    stats: Arc<RwLock<ConversionStats>>,
}

impl BitmapConverter {
    pub(crate) fn new(width: u16, height: u16) -> Self {
        Self {
            buffer_pool: Arc::new(RwLock::new(BufferPool::new(BUFFER_POOL_SIZE))),
            damage_tracker: Arc::new(RwLock::new(DamageTracker::new(width, height))),
            last_frame_hash: 0,
            stats: Arc::new(RwLock::new(ConversionStats::default())),
        }
    }

    pub(crate) fn convert_frame(
        &mut self,
        frame: &VideoFrame,
    ) -> Result<BitmapUpdate, ConversionError> {
        let start_time = Instant::now();
        if !frame.is_valid() {
            return Err(ConversionError::InvalidFrame(
                "frame is corrupt or incomplete".to_string(),
            ));
        }

        let frame_hash = calculate_frame_hash(&frame.data);
        if frame_hash == self.last_frame_hash {
            return Ok(BitmapUpdate { rectangles: vec![] });
        }
        self.last_frame_hash = frame_hash;

        {
            let mut tracker = self.damage_tracker.write();
            if frame.damage_regions.is_empty() {
                tracker.full_update = true;
            } else {
                for damage in &frame.damage_regions {
                    tracker.add_damage(Rectangle::from(*damage));
                }
            }
        }

        let regions = self.damage_tracker.read().regions();
        let rdp_format = RdpPixelFormat::from_pixel_format(frame.format);
        let output_size = calculate_output_size(frame.width, frame.height, rdp_format);
        let mut output_buffer = self.buffer_pool.write().acquire(output_size);
        output_buffer.resize(output_size, 0);

        let dst_stride = calculate_rdp_stride(frame.width, rdp_format);
        convert_format(
            &frame.data,
            &mut output_buffer,
            frame.format,
            rdp_format.pipewire_format(),
            frame.width,
            frame.height,
            frame.stride,
            dst_stride,
        )
        .map_err(|error| ConversionError::ConversionFailed(error.to_string()))?;

        let mut rectangles = Vec::with_capacity(regions.len());
        for region in regions {
            rectangles.push(extract_region(
                &output_buffer,
                region,
                frame.width,
                frame.height,
                rdp_format,
            )?);
        }

        {
            let mut stats = self.stats.write();
            stats.frames_converted += 1;
            stats.bytes_processed += frame.data_size() as u64;
            stats.conversion_time_ns += start_time.elapsed().as_nanos() as u64;
        }

        self.buffer_pool.write().release(output_buffer);
        self.damage_tracker.write().reset();

        Ok(BitmapUpdate { rectangles })
    }
}

fn calculate_frame_hash(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn extract_region(
    buffer: &[u8],
    region: Rectangle,
    frame_width: u32,
    frame_height: u32,
    format: RdpPixelFormat,
) -> Result<BitmapData, ConversionError> {
    if u32::from(region.right) > frame_width || u32::from(region.bottom) > frame_height {
        return Err(ConversionError::RegionOutOfBounds);
    }

    let bytes_per_pixel = format.bytes_per_pixel();
    let source_stride = calculate_rdp_stride(frame_width, format) as usize;
    let region_width = usize::from(region.width());
    let region_height = usize::from(region.height());
    let region_stride = region_width * bytes_per_pixel;
    let mut data = Vec::with_capacity(region_stride * region_height);

    for y in 0..region_height {
        let source_y = usize::from(region.top) + y;
        let source_x = usize::from(region.left) * bytes_per_pixel;
        let source_offset = source_y * source_stride + source_x;
        let source_end = source_offset + region_stride;
        if source_end > buffer.len() {
            return Err(ConversionError::RegionOutOfBounds);
        }
        data.extend_from_slice(&buffer[source_offset..source_end]);
    }

    Ok(BitmapData {
        rectangle: region,
        format,
        data,
    })
}

fn calculate_output_size(width: u32, height: u32, format: RdpPixelFormat) -> usize {
    calculate_rdp_stride(width, format) as usize * height as usize
}

fn calculate_rdp_stride(width: u32, format: RdpPixelFormat) -> u32 {
    align_to_boundary(width as usize * format.bytes_per_pixel(), 4) as u32
}

fn align_to_boundary(value: usize, boundary: usize) -> usize {
    value.div_ceil(boundary) * boundary
}
