//! Milestone-1 smoke: ANGLE Metal offscreen GLES context + triangle readback.
//!
//! Run: cargo run -p oxibrowser-render --features webgl --example angle_smoke
//! (requires scripts/setup-angle.sh). Implements exactly the pass criteria
//! from docs/research/2026-10-25-webgl-open-source.md step 1:
//! windowless creation, shader compile/link, known-triangle pixel readback,
//! resize + destroy/recreate repetition.

#[cfg(feature = "webgl")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use glow::HasContext;
    use oxibrowser_render::gl::{GlContext, display_info, loader};

    // Backend is selectable for experiments: OXI_ANGLE_PLATFORM=vulkan|metal
    // (default metal). See docs/designs/2026-10-25-webgl-angle.md.
    let d = display_info()?;
    println!(
        "display ok: {} / EGL {} / webgl-compat={}",
        d.egl_vendor,
        d.egl_version,
        d.webgl_compatible()
    );
    assert!(
        d.webgl_compatible(),
        "ANGLE display lacks WebGL compatibility extension"
    );

    const VS: &str = r#"
        attribute vec2 a_pos;
        varying vec2 v_pos;
        void main() { v_pos = a_pos; gl_Position = vec4(a_pos, 0.0, 1.0); }
    "#;
    const FS: &str = r#"
        precision mediump float;
        varying vec2 v_pos;
        void main() { gl_FragColor = vec4((v_pos.x + 1.0) * 0.5, (v_pos.y + 1.0) * 0.5, 0.25, 1.0); }
    "#;

    // 1. windowless creation
    let mut ctx = GlContext::new(2, 64, 64, Default::default())?;
    let gl = {
        let _ = &ctx;
        loader()?
    };
    unsafe {
        // 2. shader compile + program link
        let vs = gl
            .create_shader(glow::VERTEX_SHADER)
            .expect("create vertex shader");
        gl.shader_source(vs, VS);
        gl.compile_shader(vs);
        assert!(
            gl.get_shader_compile_status(vs),
            "vs: {}",
            gl.get_shader_info_log(vs)
        );

        let fs = gl
            .create_shader(glow::FRAGMENT_SHADER)
            .expect("create fragment shader");
        gl.shader_source(fs, FS);
        gl.compile_shader(fs);
        assert!(
            gl.get_shader_compile_status(fs),
            "fs: {}",
            gl.get_shader_info_log(fs)
        );

        let prog = gl.create_program().expect("create program");
        gl.attach_shader(prog, vs);
        gl.attach_shader(prog, fs);
        // WebGL compat enforces the attrib-0 array rule; pin a_pos to 0.
        gl.bind_attrib_location(prog, 0, "a_pos");
        gl.link_program(prog);
        assert!(
            gl.get_program_link_status(prog),
            "link: {}",
            gl.get_program_info_log(prog)
        );
        gl.use_program(Some(prog));

        let loc = gl
            .get_attrib_location(prog, "a_pos")
            .expect("a_pos location");
        let vbo = gl.create_buffer().expect("create buffer");
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let verts: [f32; 6] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0];
        let vert_bytes: [u8; 24] = {
            let mut out = [0u8; 24];
            for (i, f) in verts.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&f.to_ne_bytes());
            }
            out
        };
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &vert_bytes, glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(loc);
        gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 0, 0);

        // 3. known-triangle pixel readback
        gl.clear_color(0.0, 1.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.viewport(0, 0, 64, 64);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        let glerr = gl.get_error();
        if glerr != glow::NO_ERROR {
            panic!("GL error after draw: 0x{glerr:04X}");
        }
        gl.validate_program(prog);
        let ok = gl.get_program_parameter_i32(prog, glow::VALIDATE_STATUS);
        println!(
            "validate_status = {ok:?}, log = {:?}",
            gl.get_program_info_log(prog)
        );
        gl.finish();

        let px = ctx.read_pixels_rgba(gl, false);
        let at = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 64 + x) * 4;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        // Sample STRICTLY interior points. The triangle is
        // (-1,-1),(1,-1),(-1,1): its hypotenuse is x+y=0, and a pixel whose
        // center lands exactly on it (e.g. GL (32,32)) is fill-rule
        // excluded. bitmap(x, y) = GL(x, 63-y) (readback flips rows).
        // GL (8,8) → NDC (-0.75,-0.75) → gradient (0.125,0.125,0.25).
        let in1 = at(8, 55);
        println!("inside1 px = {in1:?} (expect [32,32,64,255])");
        assert!((in1[0] as i32 - 32).abs() <= 2, "red {in1:?}");
        assert!((in1[1] as i32 - 32).abs() <= 2, "green {in1:?}");
        assert!((in1[2] as i32 - 64).abs() <= 2, "blue {in1:?}");
        assert_eq!(in1[3], 255);
        // GL (8,40) → NDC (-0.75, 0.25) → gradient (0.125, 0.625, 0.25).
        let in2 = at(8, 23);
        println!("inside2 px = {in2:?} (expect [32,159,64,255])");
        assert!((in2[0] as i32 - 32).abs() <= 2, "red {in2:?}");
        assert!((in2[1] as i32 - 159).abs() <= 3, "green {in2:?}");
        assert!((in2[2] as i32 - 64).abs() <= 2, "blue {in2:?}");
        // GL (60,60) → NDC (0.875,0.875): outside → clear green.
        let out = at(60, 3);
        println!("outside px = {out:?} (expect green)");
        assert!(out[1] >= 250 && out[0] <= 4 && out[2] <= 4, "{out:?}");

        gl.delete_buffer(vbo);
        gl.delete_program(prog);
        gl.delete_shader(vs);
        gl.delete_shader(fs);
    }

    // 4. resize repetition
    for (w, h) in [(128, 32), (7, 5), (256, 256)] {
        ctx.resize(w, h)
            .map_err(|e| format!("resize {w}x{h}: {e}"))?;
        let gl = {
            let _ = &ctx;
            loader()?
        };
        unsafe { gl.viewport(0, 0, w as i32, h as i32) };
        let px = ctx.read_pixels_rgba(gl, false);
        assert_eq!(px.len(), (w * h * 4) as usize, "readback size after resize");
        println!("resize {w}x{h} ok");
    }

    // 5. destroy / recreate repetition
    ctx.destroy();
    for i in 0..3 {
        let c = GlContext::new(2, 32, 32, Default::default())
            .map_err(|e| format!("recreate #{i}: {e}"))?;
        c.destroy();
        println!("recreate #{i} ok");
    }

    // Bonus: GLES3/WebGL2 context creation on the same display.
    let c3 =
        GlContext::new(3, 16, 16, Default::default()).map_err(|e| format!("GLES3 ctx: {e}"))?;
    let gl = loader()?;
    let version = unsafe { gl.get_parameter_string(glow::VERSION) };
    println!("GLES3 context ok, GL_VERSION = {version}");
    c3.destroy();

    println!("ANGLE smoke: ALL PASS");
    Ok(())
}

#[cfg(not(feature = "webgl"))]
fn main() {
    eprintln!("build with --features webgl (and run scripts/setup-angle.sh first)");
    std::process::exit(1);
}
