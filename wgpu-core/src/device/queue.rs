/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[cfg(feature = "trace")]
use crate::device::trace::Action;
use crate::{
    command::{
        texture_copy_view_to_hal, validate_linear_texture_data, validate_texture_copy_range,
        CommandAllocator, CommandBuffer, CopySide, TextureCopyView, TransferError, BITS_PER_BYTE,
    },
    conv,
    device::{DeviceError, WaitIdleError},
    hub::{GfxBackend, Global, GlobalIdentityHandlerFactory, Token},
    id,
    resource::{BufferAccessError, BufferMapState, BufferUse, TextureUse},
    span,
};

use gfx_memory::{Block, Heaps, MemoryBlock};
use hal::{command::CommandBuffer as _, device::Device as _, queue::CommandQueue as _};
use smallvec::SmallVec;
use std::iter;
use thiserror::Error;

struct StagingData<B: hal::Backend> {
    buffer: B::Buffer,
    memory: MemoryBlock<B>,
    cmdbuf: B::CommandBuffer,
}

#[derive(Debug, Default)]
pub(crate) struct PendingWrites<B: hal::Backend> {
    pub command_buffer: Option<B::CommandBuffer>,
    pub temp_buffers: Vec<(B::Buffer, MemoryBlock<B>)>,
}

impl<B: hal::Backend> PendingWrites<B> {
    pub fn new() -> Self {
        PendingWrites {
            command_buffer: None,
            temp_buffers: Vec::new(),
        }
    }

    pub fn dispose(
        self,
        device: &B::Device,
        cmd_allocator: &CommandAllocator<B>,
        mem_allocator: &mut Heaps<B>,
    ) {
        if let Some(raw) = self.command_buffer {
            cmd_allocator.discard_internal(raw);
        }
        for (buffer, memory) in self.temp_buffers {
            mem_allocator.free(device, memory);
            unsafe {
                device.destroy_buffer(buffer);
            }
        }
    }

    pub fn consume_temp(&mut self, buffer: B::Buffer, memory: MemoryBlock<B>) {
        self.temp_buffers.push((buffer, memory));
    }

    fn consume(&mut self, stage: StagingData<B>) {
        self.temp_buffers.push((stage.buffer, stage.memory));
        self.command_buffer = Some(stage.cmdbuf);
    }
}

impl<B: hal::Backend> super::Device<B> {
    pub fn borrow_pending_writes(&mut self) -> &mut B::CommandBuffer {
        if self.pending_writes.command_buffer.is_none() {
            let mut cmdbuf = self.cmd_allocator.allocate_internal();
            unsafe {
                cmdbuf.begin_primary(hal::command::CommandBufferFlags::ONE_TIME_SUBMIT);
            }
            self.pending_writes.command_buffer = Some(cmdbuf);
        }
        self.pending_writes.command_buffer.as_mut().unwrap()
    }

    fn prepare_stage(&mut self, size: wgt::BufferAddress) -> Result<StagingData<B>, DeviceError> {
        let mut buffer = unsafe {
            self.raw
                .create_buffer(size, hal::buffer::Usage::TRANSFER_SRC)
                .map_err(|err| match err {
                    hal::buffer::CreationError::OutOfMemory(_) => DeviceError::OutOfMemory,
                    _ => panic!("failed to create staging buffer: {}", err),
                })?
        };
        //TODO: do we need to transition into HOST_WRITE access first?
        let requirements = unsafe { self.raw.get_buffer_requirements(&buffer) };

        let memory = self
            .mem_allocator
            .lock()
            .allocate(
                &self.raw,
                &requirements,
                gfx_memory::MemoryUsage::Staging { read_back: false },
                gfx_memory::Kind::Linear,
            )
            .map_err(DeviceError::from_heaps)?;
        unsafe {
            self.raw.set_buffer_name(&mut buffer, "<write_buffer_temp>");
            self.raw
                .bind_buffer_memory(memory.memory(), memory.segment().offset, &mut buffer)
                .map_err(DeviceError::from_bind)?;
        }

        let cmdbuf = match self.pending_writes.command_buffer.take() {
            Some(cmdbuf) => cmdbuf,
            None => {
                let mut cmdbuf = self.cmd_allocator.allocate_internal();
                unsafe {
                    cmdbuf.begin_primary(hal::command::CommandBufferFlags::ONE_TIME_SUBMIT);
                }
                cmdbuf
            }
        };
        Ok(StagingData {
            buffer,
            memory,
            cmdbuf,
        })
    }
}

#[derive(Clone, Debug, Error)]
pub enum QueueWriteError {
    #[error(transparent)]
    Queue(#[from] DeviceError),
    #[error(transparent)]
    Transfer(#[from] TransferError),
}

#[derive(Clone, Debug, Error)]
pub enum QueueSubmitError {
    #[error(transparent)]
    Queue(#[from] DeviceError),
    #[error("command buffer {0:?} is invalid")]
    InvalidCommandBuffer(id::CommandBufferId),
    #[error(transparent)]
    BufferAccess(#[from] BufferAccessError),
    #[error("swap chain output was dropped before the command buffer got submitted")]
    SwapChainOutputDropped,
    #[error("GPU got stuck :(")]
    StuckGpu,
}

//TODO: move out common parts of write_xxx.

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn queue_write_buffer<B: GfxBackend>(
        &self,
        queue_id: id::QueueId,
        buffer_id: id::BufferId,
        buffer_offset: wgt::BufferAddress,
        data: &[u8],
    ) -> Result<(), QueueWriteError> {
        span!(_guard, INFO, "Queue::write_buffer");

        let hub = B::hub(self);
        let mut token = Token::root();
        let (mut device_guard, mut token) = hub.devices.write(&mut token);
        let device = device_guard
            .get_mut(queue_id)
            .map_err(|_| DeviceError::Invalid)?;
        let (buffer_guard, _) = hub.buffers.read(&mut token);

        #[cfg(feature = "trace")]
        match device.trace {
            Some(ref trace) => {
                let mut trace = trace.lock();
                let data_path = trace.make_binary("bin", data);
                trace.add(Action::WriteBuffer {
                    id: buffer_id,
                    data: data_path,
                    range: buffer_offset..buffer_offset + data.len() as wgt::BufferAddress,
                    queued: true,
                });
            }
            None => {}
        }

        let data_size = data.len() as wgt::BufferAddress;
        if data_size == 0 {
            tracing::trace!("Ignoring write_buffer of size 0");
            return Ok(());
        }

        let mut stage = device.prepare_stage(data_size)?;
        {
            let mut mapped = stage
                .memory
                .map(&device.raw, hal::memory::Segment::ALL)
                .map_err(|err| match err {
                    hal::device::MapError::OutOfMemory(_) => DeviceError::OutOfMemory,
                    _ => panic!("failed to map buffer: {}", err),
                })?;
            unsafe { mapped.write(&device.raw, hal::memory::Segment::ALL) }
                .expect("failed to get writer to mapped staging buffer")
                .slice[..data.len()]
                .copy_from_slice(data);
        }

        let mut trackers = device.trackers.lock();
        let (dst, transition) = trackers
            .buffers
            .use_replace(&*buffer_guard, buffer_id, (), BufferUse::COPY_DST)
            .map_err(TransferError::InvalidBuffer)?;
        if !dst.usage.contains(wgt::BufferUsage::COPY_DST) {
            Err(TransferError::MissingCopyDstUsageFlag)?;
        }
        dst.life_guard.use_at(device.active_submission_index + 1);

        if data_size % wgt::COPY_BUFFER_ALIGNMENT != 0 {
            Err(TransferError::UnalignedCopySize(data_size))?
        }
        if buffer_offset % wgt::COPY_BUFFER_ALIGNMENT != 0 {
            Err(TransferError::UnalignedBufferOffset(buffer_offset))?
        }
        if buffer_offset + data_size > dst.size {
            Err(TransferError::BufferOverrun {
                start_offset: buffer_offset,
                end_offset: buffer_offset + data_size,
                buffer_size: dst.size,
                side: CopySide::Destination,
            })?
        }

        let region = hal::command::BufferCopy {
            src: 0,
            dst: buffer_offset,
            size: data.len() as _,
        };
        unsafe {
            stage.cmdbuf.pipeline_barrier(
                super::all_buffer_stages()..hal::pso::PipelineStage::TRANSFER,
                hal::memory::Dependencies::empty(),
                iter::once(hal::memory::Barrier::Buffer {
                    states: hal::buffer::Access::HOST_WRITE..hal::buffer::Access::TRANSFER_READ,
                    target: &stage.buffer,
                    range: hal::buffer::SubRange::WHOLE,
                    families: None,
                })
                .chain(transition.map(|pending| pending.into_hal(dst))),
            );
            stage
                .cmdbuf
                .copy_buffer(&stage.buffer, &dst.raw, iter::once(region));
        }

        device.pending_writes.consume(stage);

        Ok(())
    }

    pub fn queue_write_texture<B: GfxBackend>(
        &self,
        queue_id: id::QueueId,
        destination: &TextureCopyView,
        data: &[u8],
        data_layout: &wgt::TextureDataLayout,
        size: &wgt::Extent3d,
    ) -> Result<(), QueueWriteError> {
        span!(_guard, INFO, "Queue::write_texture");

        let hub = B::hub(self);
        let mut token = Token::root();
        let (mut device_guard, mut token) = hub.devices.write(&mut token);
        let device = device_guard
            .get_mut(queue_id)
            .map_err(|_| DeviceError::Invalid)?;
        let (texture_guard, _) = hub.textures.read(&mut token);
        let (image_layers, image_range, image_offset) =
            texture_copy_view_to_hal(destination, size, &*texture_guard)?;

        #[cfg(feature = "trace")]
        match device.trace {
            Some(ref trace) => {
                let mut trace = trace.lock();
                let data_path = trace.make_binary("bin", data);
                trace.add(Action::WriteTexture {
                    to: destination.clone(),
                    data: data_path,
                    layout: data_layout.clone(),
                    size: *size,
                });
            }
            None => {}
        }

        if size.width == 0 || size.height == 0 || size.width == 0 {
            tracing::trace!("Ignoring write_texture of size 0");
            return Ok(());
        }

        let texture_format = texture_guard.get(destination.texture).unwrap().format;
        let bytes_per_block = conv::map_texture_format(texture_format, device.private_features)
            .surface_desc()
            .bits as u32
            / BITS_PER_BYTE;
        validate_linear_texture_data(
            data_layout,
            texture_format,
            data.len() as wgt::BufferAddress,
            CopySide::Source,
            bytes_per_block as wgt::BufferAddress,
            size,
        )?;
        let (block_width, block_height) = conv::texture_block_size(texture_format);
        let width_blocks = size.width / block_width;
        let height_blocks = size.height / block_width;

        let texel_rows_per_image = data_layout.rows_per_image;
        let block_rows_per_image = data_layout.rows_per_image / block_height;

        let bytes_per_row_alignment = get_lowest_common_denom(
            device.hal_limits.optimal_buffer_copy_pitch_alignment as u32,
            bytes_per_block,
        );
        let stage_bytes_per_row = align_to(bytes_per_block * width_blocks, bytes_per_row_alignment);

        let block_rows_in_copy = (size.depth - 1) * block_rows_per_image + height_blocks;
        let stage_size = stage_bytes_per_row as u64 * block_rows_in_copy as u64;
        let mut stage = device.prepare_stage(stage_size)?;

        let mut trackers = device.trackers.lock();
        let (dst, transition) = trackers
            .textures
            .use_replace(
                &*texture_guard,
                destination.texture,
                image_range,
                TextureUse::COPY_DST,
            )
            .unwrap();

        if !dst.usage.contains(wgt::TextureUsage::COPY_DST) {
            Err(TransferError::MissingCopyDstUsageFlag)?
        }
        validate_texture_copy_range(
            destination,
            dst.format,
            dst.kind,
            CopySide::Destination,
            size,
        )?;
        dst.life_guard.use_at(device.active_submission_index + 1);

        {
            let mut mapped = stage
                .memory
                .map(&device.raw, hal::memory::Segment::ALL)
                .map_err(|err| match err {
                    hal::device::MapError::OutOfMemory(_) => DeviceError::OutOfMemory,
                    _ => panic!("failed to map staging buffer: {}", err),
                })?;
            let mapping = unsafe { mapped.write(&device.raw, hal::memory::Segment::ALL) }
                .expect("failed to get writer to mapped staging buffer");
            if stage_bytes_per_row == data_layout.bytes_per_row {
                // Fast path if the data isalready being aligned optimally.
                mapping.slice[..stage_size as usize].copy_from_slice(data);
            } else {
                // Copy row by row into the optimal alignment.
                let copy_bytes_per_row =
                    stage_bytes_per_row.min(data_layout.bytes_per_row) as usize;
                for layer in 0..size.depth {
                    let rows_offset = layer * block_rows_per_image;
                    for row in 0..height_blocks {
                        let data_offset =
                            (rows_offset + row) as usize * data_layout.bytes_per_row as usize;
                        let stage_offset =
                            (rows_offset + row) as usize * stage_bytes_per_row as usize;
                        mapping.slice[stage_offset..stage_offset + copy_bytes_per_row]
                            .copy_from_slice(&data[data_offset..data_offset + copy_bytes_per_row]);
                    }
                }
            }
        }

        let region = hal::command::BufferImageCopy {
            buffer_offset: 0,
            buffer_width: (stage_bytes_per_row / bytes_per_block) * block_width,
            buffer_height: texel_rows_per_image,
            image_layers,
            image_offset,
            image_extent: conv::map_extent(size, dst.dimension),
        };
        unsafe {
            stage.cmdbuf.pipeline_barrier(
                super::all_image_stages() | hal::pso::PipelineStage::HOST
                    ..hal::pso::PipelineStage::TRANSFER,
                hal::memory::Dependencies::empty(),
                iter::once(hal::memory::Barrier::Buffer {
                    states: hal::buffer::Access::HOST_WRITE..hal::buffer::Access::TRANSFER_READ,
                    target: &stage.buffer,
                    range: hal::buffer::SubRange::WHOLE,
                    families: None,
                })
                .chain(transition.map(|pending| pending.into_hal(dst))),
            );
            stage.cmdbuf.copy_buffer_to_image(
                &stage.buffer,
                &dst.raw,
                hal::image::Layout::TransferDstOptimal,
                iter::once(region),
            );
        }

        device.pending_writes.consume(stage);

        Ok(())
    }

    pub fn queue_submit<B: GfxBackend>(
        &self,
        queue_id: id::QueueId,
        command_buffer_ids: &[id::CommandBufferId],
    ) -> Result<(), QueueSubmitError> {
        span!(_guard, INFO, "Queue::submit");

        let hub = B::hub(self);

        let callbacks = {
            let mut token = Token::root();
            let (mut device_guard, mut token) = hub.devices.write(&mut token);
            let device = device_guard
                .get_mut(queue_id)
                .map_err(|_| DeviceError::Invalid)?;
            let pending_write_command_buffer =
                device
                    .pending_writes
                    .command_buffer
                    .take()
                    .map(|mut comb_raw| unsafe {
                        comb_raw.finish();
                        comb_raw
                    });
            device.temp_suspected.clear();
            device.active_submission_index += 1;
            let submit_index = device.active_submission_index;

            let fence = {
                let mut signal_swapchain_semaphores = SmallVec::<[_; 1]>::new();
                let (mut swap_chain_guard, mut token) = hub.swap_chains.write(&mut token);
                let (mut command_buffer_guard, mut token) = hub.command_buffers.write(&mut token);

                {
                    let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
                    let (compute_pipe_guard, mut token) = hub.compute_pipelines.read(&mut token);
                    let (render_pipe_guard, mut token) = hub.render_pipelines.read(&mut token);
                    let (mut buffer_guard, mut token) = hub.buffers.write(&mut token);
                    let (texture_guard, mut token) = hub.textures.read(&mut token);
                    let (texture_view_guard, mut token) = hub.texture_views.read(&mut token);
                    let (sampler_guard, _) = hub.samplers.read(&mut token);

                    //Note: locking the trackers has to be done after the storages
                    let mut trackers = device.trackers.lock();

                    //TODO: if multiple command buffers are submitted, we can re-use the last
                    // native command buffer of the previous chain instead of always creating
                    // a temporary one, since the chains are not finished.

                    // finish all the command buffers first
                    for &cmb_id in command_buffer_ids {
                        let cmdbuf = command_buffer_guard
                            .get_mut(cmb_id)
                            .map_err(|_| QueueSubmitError::InvalidCommandBuffer(cmb_id))?;
                        #[cfg(feature = "trace")]
                        match device.trace {
                            Some(ref trace) => trace.lock().add(Action::Submit(
                                submit_index,
                                cmdbuf.commands.take().unwrap(),
                            )),
                            None => (),
                        };

                        if let Some((sc_id, fbo)) = cmdbuf.used_swap_chain.take() {
                            let sc = &mut swap_chain_guard[sc_id.value];
                            sc.active_submission_index = submit_index;
                            if sc.acquired_view_id.is_none() {
                                return Err(QueueSubmitError::SwapChainOutputDropped);
                            }
                            // For each swapchain, we only want to have at most 1 signaled semaphore.
                            if sc.acquired_framebuffers.is_empty() {
                                // Only add a signal if this is the first time for this swapchain
                                // to be used in the submission.
                                signal_swapchain_semaphores.push(sc_id.value);
                            }
                            sc.acquired_framebuffers.push(fbo);
                        }

                        // optimize the tracked states
                        cmdbuf.trackers.optimize();

                        // update submission IDs
                        for id in cmdbuf.trackers.buffers.used() {
                            let buffer = &mut buffer_guard[id];
                            if !buffer.life_guard.use_at(submit_index) {
                                if let BufferMapState::Active { .. } = buffer.map_state {
                                    tracing::warn!("Dropped buffer has a pending mapping.");
                                    super::unmap_buffer(&device.raw, buffer)?;
                                }
                                device.temp_suspected.buffers.push(id);
                            } else {
                                match buffer.map_state {
                                    BufferMapState::Idle => (),
                                    _ => panic!("Buffer {:?} is still mapped", id),
                                }
                            }
                        }
                        for id in cmdbuf.trackers.textures.used() {
                            if !texture_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.textures.push(id);
                            }
                        }
                        for id in cmdbuf.trackers.views.used() {
                            if !texture_view_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.texture_views.push(id);
                            }
                        }
                        for id in cmdbuf.trackers.bind_groups.used() {
                            if !bind_group_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.bind_groups.push(id);
                            }
                        }
                        for id in cmdbuf.trackers.samplers.used() {
                            if !sampler_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.samplers.push(id);
                            }
                        }
                        for id in cmdbuf.trackers.compute_pipes.used() {
                            if !compute_pipe_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.compute_pipelines.push(id);
                            }
                        }
                        for id in cmdbuf.trackers.render_pipes.used() {
                            if !render_pipe_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.render_pipelines.push(id);
                            }
                        }

                        // execute resource transitions
                        let mut transit = device.cmd_allocator.extend(cmdbuf);
                        unsafe {
                            // the last buffer was open, closing now
                            cmdbuf.raw.last_mut().unwrap().finish();
                            transit
                                .begin_primary(hal::command::CommandBufferFlags::ONE_TIME_SUBMIT);
                        }
                        tracing::trace!("Stitching command buffer {:?} before submission", cmb_id);
                        CommandBuffer::insert_barriers(
                            &mut transit,
                            &mut *trackers,
                            &cmdbuf.trackers,
                            &*buffer_guard,
                            &*texture_guard,
                        );
                        unsafe {
                            transit.finish();
                        }
                        cmdbuf.raw.insert(0, transit);
                    }

                    tracing::trace!("Device after submission {}: {:#?}", submit_index, trackers);
                }

                // now prepare the GPU submission
                let fence = device
                    .raw
                    .create_fence(false)
                    .or(Err(DeviceError::OutOfMemory))?;
                let submission = hal::queue::Submission {
                    command_buffers: pending_write_command_buffer.as_ref().into_iter().chain(
                        command_buffer_ids
                            .iter()
                            .flat_map(|&cmb_id| &command_buffer_guard.get(cmb_id).unwrap().raw),
                    ),
                    wait_semaphores: Vec::new(),
                    signal_semaphores: signal_swapchain_semaphores
                        .into_iter()
                        .map(|sc_id| &swap_chain_guard[sc_id].semaphore),
                };

                unsafe {
                    device.queue_group.queues[0].submit(submission, Some(&fence));
                }
                fence
            };

            if let Some(comb_raw) = pending_write_command_buffer {
                device
                    .cmd_allocator
                    .after_submit_internal(comb_raw, submit_index);
            }

            let callbacks = match device.maintain(&hub, false, &mut token) {
                Ok(callbacks) => callbacks,
                Err(WaitIdleError::Device(err)) => return Err(QueueSubmitError::Queue(err)),
                Err(WaitIdleError::StuckGpu) => return Err(QueueSubmitError::StuckGpu),
            };
            super::Device::lock_life_internal(&device.life_tracker, &mut token).track_submission(
                submit_index,
                fence,
                &device.temp_suspected,
                device.pending_writes.temp_buffers.drain(..),
            );

            // finally, return the command buffers to the allocator
            for &cmb_id in command_buffer_ids {
                let (cmd_buf, _) = hub.command_buffers.unregister(cmb_id, &mut token);
                device.cmd_allocator.after_submit(cmd_buf, submit_index);
            }

            callbacks
        };

        super::fire_map_callbacks(callbacks);

        Ok(())
    }
}

fn get_lowest_common_denom(a: u32, b: u32) -> u32 {
    let gcd = if a >= b {
        get_greatest_common_divisor(a, b)
    } else {
        get_greatest_common_divisor(b, a)
    };
    a * b / gcd
}

fn get_greatest_common_divisor(mut a: u32, mut b: u32) -> u32 {
    assert!(a >= b);
    loop {
        let c = a % b;
        if c == 0 {
            return b;
        } else {
            a = b;
            b = c;
        }
    }
}

fn align_to(value: u32, alignment: u32) -> u32 {
    match value % alignment {
        0 => value,
        other => value - other + alignment,
    }
}

#[test]
fn test_lcd() {
    assert_eq!(get_lowest_common_denom(2, 2), 2);
    assert_eq!(get_lowest_common_denom(2, 3), 6);
    assert_eq!(get_lowest_common_denom(6, 4), 12);
}

#[test]
fn test_gcd() {
    assert_eq!(get_greatest_common_divisor(5, 1), 1);
    assert_eq!(get_greatest_common_divisor(4, 2), 2);
    assert_eq!(get_greatest_common_divisor(6, 4), 2);
    assert_eq!(get_greatest_common_divisor(7, 7), 7);
}
