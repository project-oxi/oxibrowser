//! Measure ANGLE display/context startup, synchronous readback, RSS, and
//! repeated context lifetime on the selected backend.
//!
//! Run the hardware backend with:
//! `cargo run -p oxibrowser-render --features webgl --example angle_bench`
//! Run SwiftShader with:
//! `OXI_ANGLE_PLATFORM=vulkan VK_ICD_FILENAMES=<manifest> cargo run -p oxibrowser-render --features webgl --example angle_bench`

#[cfg(feature = "webgl")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;
    use std::time::Instant;

    use glow::HasContext;
    use oxibrowser_render::gl::{ContextAttrs, GlContext, display_info, loader};

    const WIDTH: u32 = 256;
    const HEIGHT: u32 = 256;
    const READBACK_SAMPLES: usize = 64;
    const CONTEXT_CYCLES: usize = 64;

    fn rss_bytes() -> Result<u64, Box<dyn std::error::Error>> {
        let pid = std::process::id().to_string();
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()?;
        if !output.status.success() {
            return Err(format!("ps exited with {}", output.status).into());
        }
        let kilobytes: u64 = std::str::from_utf8(&output.stdout)?.trim().parse()?;
        Ok(kilobytes.saturating_mul(1024))
    }

    fn percentile(samples: &[u128], percent: usize) -> u128 {
        samples[(samples.len() * percent / 100).min(samples.len() - 1)]
    }

    let rss_before = rss_bytes()?;
    let display_start = Instant::now();
    let display = display_info()?;
    let display_init_us = display_start.elapsed().as_micros();

    let context_start = Instant::now();
    let context = GlContext::new(2, WIDTH, HEIGHT, ContextAttrs::default())?;
    let context_init_us = context_start.elapsed().as_micros();
    let gl = loader()?;
    let renderer = unsafe { gl.get_parameter_string(glow::RENDERER) };
    let vendor = unsafe { gl.get_parameter_string(glow::VENDOR) };
    println!(
        "BACKEND egl_vendor={:?} egl_version={:?}",
        display.egl_vendor, display.egl_version
    );
    println!("BACKEND gl_vendor={vendor:?} gl_renderer={renderer:?}");
    println!("METRIC display_init_us={display_init_us}");
    println!("METRIC first_context_init_us={context_init_us}");
    println!("METRIC rss_before_bytes={rss_before}");
    println!("METRIC rss_after_first_context_bytes={}", rss_bytes()?);

    unsafe {
        gl.viewport(0, 0, WIDTH as i32, HEIGHT as i32);
        gl.clear_color(0.25, 0.5, 0.75, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.finish();
    }

    let expected_len = (WIDTH * HEIGHT * 4) as usize;
    let mut readback_us = Vec::with_capacity(READBACK_SAMPLES);
    for _ in 0..READBACK_SAMPLES {
        let start = Instant::now();
        let pixels = context.read_pixels_rgba(gl, false);
        readback_us.push(start.elapsed().as_micros());
        if pixels.len() != expected_len || pixels[0] == 0 {
            return Err(format!(
                "invalid readback: {} bytes, first red byte {}",
                pixels.len(),
                pixels.first().copied().unwrap_or(0)
            )
            .into());
        }
    }
    let gl_error = unsafe { gl.get_error() };
    if gl_error != glow::NO_ERROR {
        return Err(format!("GL error after readback: 0x{gl_error:04X}").into());
    }
    readback_us.sort_unstable();
    let readback_avg_us = readback_us.iter().sum::<u128>() / readback_us.len() as u128;
    println!("METRIC readback_samples={READBACK_SAMPLES}");
    println!("METRIC readback_256x256_avg_us={readback_avg_us}");
    println!(
        "METRIC readback_256x256_p95_us={}",
        percentile(&readback_us, 95)
    );
    println!("METRIC rss_after_readback_bytes={}", rss_bytes()?);

    let mut cycle_us = Vec::with_capacity(CONTEXT_CYCLES);
    for cycle in 0..CONTEXT_CYCLES {
        let start = Instant::now();
        let cycle_context = GlContext::new(2, 64, 64, ContextAttrs::default())?;
        let cycle_gl = loader()?;
        unsafe {
            cycle_gl.viewport(0, 0, 64, 64);
            cycle_gl.clear_color(0.0, 0.0, 0.0, 1.0);
            cycle_gl.clear(glow::COLOR_BUFFER_BIT);
            cycle_gl.finish();
        }
        let pixels = cycle_context.read_pixels_rgba(cycle_gl, false);
        if pixels.len() != 64 * 64 * 4 {
            return Err(format!("cycle {cycle} returned {} bytes", pixels.len()).into());
        }
        cycle_context.destroy();
        cycle_us.push(start.elapsed().as_micros());

        if (cycle + 1) % 16 == 0 {
            println!(
                "METRIC rss_after_context_cycles_{}_bytes={}",
                cycle + 1,
                rss_bytes()?
            );
        }
    }
    cycle_us.sort_unstable();
    println!("METRIC context_cycles={CONTEXT_CYCLES}");
    println!(
        "METRIC context_cycle_avg_us={}",
        cycle_us.iter().sum::<u128>() / cycle_us.len() as u128
    );
    println!("METRIC context_cycle_p95_us={}", percentile(&cycle_us, 95));
    println!("METRIC rss_after_context_cycles_bytes={}", rss_bytes()?);

    context.destroy();
    println!("ANGLE benchmark: ALL PASS");
    Ok(())
}

#[cfg(not(feature = "webgl"))]
fn main() {
    eprintln!("build with --features webgl (and provision ANGLE first)");
    std::process::exit(1);
}
