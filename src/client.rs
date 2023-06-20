use std::{
    fs, io,
    net::{Shutdown, TcpStream},
    path::Path,
};

use dialog::DialogBox;
use glow::HasContext;
use imgui::Context;
use imgui_glow_renderer::AutoRenderer;
use imgui_sdl2_support::SdlPlatform;
use p2p_service::{read_bytes, read_usize, write_string, Chunk, SERVER_ADDR};
use sdl2::{
    event::Event,
    video::{GLProfile, Window},
};

const FRAMES_BEFORE_KEEP_ALIVE: usize = 16;

// Create a new glow context.
fn glow_context(window: &Window) -> glow::Context {
    unsafe {
        glow::Context::from_loader_function(|s| window.subsystem().gl_get_proc_address(s) as _)
    }
}

fn send_file(file_name: &str, stream: &TcpStream) -> io::Result<()> {
    let mut chunk = Chunk::<1024>::new(stream);
    let file_name = String::from(file_name);

    chunk.write_and_send(&0u8.to_le_bytes())?;
    write_string(&mut chunk, &file_name)?;

    p2p_service::send_file(&mut chunk, &file_name)?;

    println!("File sent successfully!");

    Ok(())
}

fn get_file(stream: &TcpStream, file_name: &str) -> io::Result<Option<Vec<u8>>> {
    let mut chunk = Chunk::<1024>::new(stream);

    chunk.write_and_send(&1u8.to_le_bytes())?;
    write_string(&mut chunk, file_name)?;

    let file_size = read_usize(&mut chunk);
    p2p_service::receive_file(&mut chunk, file_size)
}

fn fetch_files(stream: &TcpStream) -> io::Result<Vec<String>> {
    let mut chunk = Chunk::<1024>::new(stream);
    chunk.write_and_send(&2u8.to_le_bytes())?;

    chunk.read_stream(8)?;
    let count = usize::from_le_bytes(chunk.to_byte_array::<8>());

    let mut files = Vec::new();
    for _ in 0..count {
        let bytes = read_bytes(&mut chunk)?.unwrap();
        let file_name = String::from_utf8_lossy(&bytes).to_string();
        files.push(file_name);
    }

    Ok(files)
}

fn run(stream: TcpStream) {
    /* initialize SDL and its video subsystem */
    let sdl = sdl2::init().unwrap();
    let video_subsystem = sdl.video().unwrap();

    /* hint SDL to initialize an OpenGL 3.3 core profile context */
    let gl_attr = video_subsystem.gl_attr();

    gl_attr.set_context_version(3, 3);
    gl_attr.set_context_profile(GLProfile::Core);

    let window_size = (720, 480);

    /* create a new window, be sure to call opengl method on the builder when using glow! */
    let window = video_subsystem
        .window("P2P Client", window_size.0, window_size.1)
        .allow_highdpi()
        .opengl()
        .position_centered()
        .build()
        .unwrap();

    /* create a new OpenGL context and make it current */
    let gl_context = window.gl_create_context().unwrap();
    window.gl_make_current(&gl_context).unwrap();

    /* enable vsync to cap framerate */
    window.subsystem().gl_set_swap_interval(1).unwrap();

    /* create new glow and imgui contexts */
    let gl = glow_context(&window);

    /* create context */
    let mut imgui = Context::create();

    /* disable creation of files on disc */
    imgui.set_ini_filename(None);
    imgui.set_log_filename(None);

    /* setup platform and renderer, and fonts to imgui */
    imgui
        .fonts()
        .add_font(&[imgui::FontSource::DefaultFontData { config: None }]);

    /* create platform and renderer */
    let mut platform = SdlPlatform::init(&mut imgui);
    let mut renderer = AutoRenderer::initialize(gl, &mut imgui).unwrap();

    /* start main loop */
    let mut event_pump = sdl.event_pump().unwrap();
    let mut selected_file: Option<String> = None;
    let mut frames_before_send = 0usize;

    let mut chunk = Chunk::<1024>::new(&stream);
    let mut cached_files = fetch_files(&stream).unwrap();

    'main: loop {
        for event in event_pump.poll_iter() {
            /* pass all events to imgui platfrom */
            platform.handle_event(&mut imgui, &event);

            if let Event::Quit { .. } = event {
                break 'main;
            }
        }

        frames_before_send += 1;
        if frames_before_send >= FRAMES_BEFORE_KEEP_ALIVE {
            frames_before_send = 0;
            chunk.write_and_send(&3u8.to_le_bytes()).unwrap();
        }

        /* call prepare_frame before calling imgui.new_frame() */
        platform.prepare_frame(&mut imgui, &window, &event_pump);

        let ui = imgui.new_frame();
        /* create imgui UI here */
        ui.window("File Management")
            .movable(false)
            .collapsible(false)
            .resizable(false)
            .size(
                [window_size.0 as f32, window_size.1 as f32],
                imgui::Condition::FirstUseEver,
            )
            .position([0.0, 0.0], imgui::Condition::FirstUseEver)
            .build(|| {
                if ui.button("Open Files...") {
                    let d = dialog::FileSelection::new(".");
                    selected_file = d.show().expect("Could not open dialog");
                }
                ui.separator();
                ui.text(format!("Selected file: '{selected_file:#?}'"));

                if ui.button("Upload") {
                    if let Some(file) = &selected_file {
                        if let Err(err) = send_file(file, &stream) {
                            show_msg_box(&format!("Could not send file over network: '{err}'"));
                        } else {
                            show_msg_box("File uploaded!");
                            cached_files.push(
                                Path::new(&file)
                                    .file_name()
                                    .unwrap()
                                    .to_str()
                                    .unwrap()
                                    .to_string(),
                            );
                            selected_file = None;
                        }
                    }
                }

                ui.separator();
                ui.text("Server Files");

                if ui.button("Fetch") {
                    match fetch_files(&stream) {
                        Ok(files) => {
                            cached_files.clear();
                            cached_files = files;
                        }
                        Err(err) => show_msg_box(&format!("Could not fetch files: '{err}'")),
                    }
                }

                ui.separator();

                for file in &cached_files {
                    if ui.button(file) {
                        match get_file(&stream, file) {
                            Ok(contents) => {
                                if let Some(contents) = contents {
                                    if let Ok(_) = fs::write(file, contents) {
                                        show_msg_box("File downloaded!");
                                    }
                                }
                            }
                            Err(err) => show_msg_box(&format!("Could not download file: '{err}'")),
                        }
                    }
                }
            });

        /* render */
        let draw_data = imgui.render();

        unsafe { renderer.gl_context().clear(glow::COLOR_BUFFER_BIT) };
        renderer.render(draw_data).unwrap();

        window.gl_swap_window();
    }

    stream
        .shutdown(Shutdown::Both)
        .expect("Stream shutdown failed");
}

fn show_msg_box(msg: &str) {
    let msg = dialog::Message::new(msg);
    msg.show_with(dialog::default_backend())
        .expect("Could not show message");
}

fn main() {
    if let Ok(stream) = TcpStream::connect(SERVER_ADDR) {
        run(stream);
    } else {
        show_msg_box("Could't connect to server!");
    }
}
