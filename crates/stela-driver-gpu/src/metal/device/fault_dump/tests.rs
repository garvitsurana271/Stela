use super::super::{
    fixture::{LIBRARY, TestDevice, device},
    pipelines::{ComputePipelineRequest, LinkedFunctions},
};
use crate::metal::{
    abi::{CpuCacheMode, DispatchType, HazardTrackingMode, ResourceOptions, StorageMode},
    capabilities::Size,
    error::Error,
    fault_dump::{HEX_LINE_BYTES, ShaderImage},
};
use alloc::{format, string::String, vec, vec::Vec};

/// Commits a command buffer that dispatches a compute pipeline of the
/// function `kernel`; answers the pipeline's id and the serial.
fn dispatched(device: &mut TestDevice) -> (u64, u64) {
    let kernel = device.new_function(LIBRARY, "kernel", Some(&[])).unwrap();
    let pipeline = device
        .new_compute_pipeline(&ComputePipelineRequest {
            compute_function: kernel,
            threadgroup_size_is_multiple_of_thread_execution_width: false,
            max_total_threads_per_threadgroup: 0,
            stage_input: None,
            linked_functions: LinkedFunctions::default(),
            support_indirect_command_buffers: false,
            archives: Default::default(),
        })
        .unwrap();
    let state = device.compute_pipeline_state(pipeline).unwrap().clone();
    let options = ResourceOptions {
        cpu_cache_mode: CpuCacheMode::DefaultCache,
        storage_mode: StorageMode::Private,
        hazard_tracking_mode: HazardTrackingMode::Default,
    };
    let buffer = device.new_buffer(4096, options).unwrap();
    let buffer = device.buffer(buffer).unwrap();
    let mut commands = device.new_command_buffer();
    commands.begin_compute(DispatchType::Serial).unwrap();
    commands.set_compute_pipeline_state(&state).unwrap();
    commands.set_compute_buffer(Some(buffer), 0, 0).unwrap();
    let threads = Size {
        width: 64,
        height: 1,
        depth: 1,
    };
    commands
        .dispatch_threadgroups(&mut device.resolver(), [1, 1, 1], threads)
        .unwrap();
    commands.end_encoding(&mut device.resolver()).unwrap();
    let serial = device.commit(commands).unwrap();
    (state.id, serial)
}

fn lose(device: &mut TestDevice) {
    device.backend().semaphore_error = Some(Error::DeviceLost);
    assert_eq!(device.poll(), Err(Error::DeviceLost));
}

fn kernel_image() -> ShaderImage {
    ShaderImage {
        stage: String::from("compute"),
        code: (0..100).collect(),
        statistics: vec![(String::from("Number of GPRs"), 8)],
        ..ShaderImage::default()
    }
}

/// The first fault names the unfinished command buffer, the pipeline it
/// bound by its function, each stage the driver reports and its code in
/// hex; a later fault names the same without the hex.
#[test]
fn a_lost_device_dumps_the_pipelines_its_unfinished_work_bound() {
    let mut device = device();
    device.backend().shader_images = vec![kernel_image()];
    let (id, serial) = dispatched(&mut device);
    lose(&mut device);

    let lines = device.fault_dump(1);
    assert_eq!(
        lines[0],
        format!(
            "fault dump 1/3: the device was lost running command buffer {serial}, which bound 1 \
             pipeline: compute {id}"
        )
    );
    assert!(lines[1].starts_with(&format!("fault dump 1 command buffer {serial}: ")));
    let stage = format!("fault dump 1 compute {id} variant 0 compute (kernel): code=100 sha256=");
    assert!(lines[2].starts_with(&stage), "{}", lines[2]);
    assert!(lines[2].ends_with(" gprs=8 sph=none"), "{}", lines[2]);
    let hex: Vec<&String> = lines[3..].iter().collect();
    assert_eq!(hex.len(), 100usize.div_ceil(HEX_LINE_BYTES));
    assert!(hex[0].starts_with(&format!(
        "fault dump 1 compute {id} variant 0 compute code+0x0000: 000102"
    )));
    assert!(hex[1].starts_with(&format!(
        "fault dump 1 compute {id} variant 0 compute code+0x0040: 404142"
    )));

    let later = device.fault_dump(2);
    assert_eq!(
        later.len(),
        3,
        "no hex past the boot's first fault: {later:?}"
    );
    assert!(later[2].starts_with(&format!("fault dump 2 compute {id} variant 0 compute")));
}

/// A driver that reports no shaders still has its pipelines named.
#[test]
fn a_driver_without_shader_images_still_names_the_pipelines() {
    let mut device = device();
    let (id, _) = dispatched(&mut device);
    lose(&mut device);
    assert_eq!(
        device.fault_dump(1)[2..],
        [format!(
            "fault dump 1 compute {id} variant 0 (kernel): the driver reports no shaders"
        )]
    );
}

/// The span whose start the GPU wrote and whose end it did not is named
/// with the pipelines it bound; a command buffer the GPU had not started,
/// one whose spans all ended, and a driver that reads nothing on a lost
/// device are each said so.
#[test]
fn a_lost_device_says_which_span_of_its_unfinished_work_never_ended() {
    let mut device = device();
    let (id, serial) = dispatched(&mut device);
    lose(&mut device);
    let opening = format!("fault dump 1 command buffer {serial}: ");

    device.backend().queries_written = Some(1);
    let line = &device.fault_dump(1)[1];
    assert!(line.starts_with(&format!("{opening}span 0 of ")), "{line}");
    assert!(
        line.contains("started and never ended [Compute 0x0 draws=0 dispatches=1"),
        "{line}"
    );
    assert!(
        line.ends_with(&format!(
            "indirect=0 groups=1], binding compute {id} (kernel)"
        )),
        "{line}"
    );

    device.backend().queries_written = Some(0);
    assert_eq!(
        device.fault_dump(1)[1],
        format!("{opening}not started, no timestamp written (the GPU ran work before it)")
    );

    device.backend().queries_written = Some(u32::MAX);
    let line = &device.fault_dump(1)[1];
    assert!(
        line.starts_with(&format!("{opening}all "))
            && line.ends_with(" spans ended (its own work finished)"),
        "{line}"
    );

    device.backend().queries_written = None;
    assert_eq!(
        device.fault_dump(1)[1],
        format!(
            "{opening}its timestamps are unreadable on the lost device: \
             Unsupported(\"query results without waiting\")"
        )
    );
}

/// Work the GPU finished is not what it was running when it was lost.
#[test]
fn finished_work_is_not_named() {
    let mut device = device();
    device.backend().shader_images = vec![kernel_image()];
    let (_, serial) = dispatched(&mut device);
    let timeline = device.timeline;
    device.backend().complete(timeline, serial);
    assert_eq!(device.poll(), Ok(serial));
    // With nothing in flight a poll asks nothing; the platform's look
    // learns of the loss.
    device.mark_lost();
    assert_eq!(
        device.fault_dump(1),
        ["fault dump 1/3: the device was lost with no submitted command buffer unfinished"]
    );
}
