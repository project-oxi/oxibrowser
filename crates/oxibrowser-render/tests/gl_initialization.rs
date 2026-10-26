#![cfg(feature = "webgl")]

use std::sync::{Arc, Barrier};

use glow::HasContext;
use oxibrowser_render::gl::{ContextAttrs, GlContext, loader};

// A separate test executable keeps the shared ANGLE display cold. Concurrent
// first contexts must receive complete caps, not an in-progress Metal cache.
#[test]
fn cold_parallel_contexts_have_valid_capabilities_and_pixels() {
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|index| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let context = GlContext::new(2, 8, 8, ContextAttrs::default()).unwrap();
                let gl = loader().unwrap();
                let green = (index % 2) as f32;
                let blue = (index / 2 % 2) as f32;
                for _ in 0..16 {
                    let precision = unsafe {
                        gl.get_shader_precision_format(glow::VERTEX_SHADER, glow::HIGH_FLOAT)
                    }
                    .expect("valid HIGH_FLOAT capability");
                    assert!(
                        precision.range_max > 0 && precision.precision > 0,
                        "worker {index}: {precision:?}"
                    );
                    unsafe {
                        gl.clear_color(1.0, green, blue, 1.0);
                        gl.clear(glow::COLOR_BUFFER_BIT);
                    }
                    let pixels = context.read_pixels_rgba(gl, false);
                    assert_eq!(
                        &pixels[..4],
                        &[255, (green * 255.0) as u8, (blue * 255.0) as u8, 255],
                        "worker {index}"
                    );
                    assert_eq!(unsafe { gl.get_error() }, glow::NO_ERROR);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}
