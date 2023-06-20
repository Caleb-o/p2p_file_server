use std::{
    collections::HashSet,
    fs, io,
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
};

use p2p_service::{
    read_string, read_usize, receive_file, send_file, write_string, write_usize, Chunk,
    SharedFiles, ThreadPool, SERVER_ADDR,
};

const SERVER_FILES: &'static str = "server_files";
const THREAD_COUNT: usize = 8;

fn add_file<const N: usize>(chunk: &mut Chunk<N>, shared_files: SharedFiles) -> io::Result<()> {
    let file_name = read_string(chunk)?;
    let file_size = read_usize(chunk);

    println!("Receiving file: \"{file_name}\" ({file_size} bytes)");

    let contents = receive_file(chunk, file_size)?;
    let file_name = Path::new(&file_name)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    if let Some(contents) = contents {
        fs::write(format!("{SERVER_FILES}/{file_name}"), contents)?;

        // Add filename to index
        let mut shared_files = shared_files.lock().unwrap();
        shared_files.insert(file_name);
    }

    println!("File received successfully!");
    Ok(())
}

fn get_file<const N: usize>(chunk: &mut Chunk<N>) -> io::Result<()> {
    let file_name = format!("{SERVER_FILES}/{}", read_string(chunk)?);

    if !Path::new(&file_name).exists() {
        write_usize(chunk, 0)?;
        return Ok(());
    }

    println!("Sending file: \"{file_name}\"");

    send_file(chunk, &file_name)?;

    println!("File sent successfully!");
    Ok(())
}

fn fetch_files<const N: usize>(chunk: &mut Chunk<N>, shared_files: SharedFiles) -> io::Result<()> {
    let shared_files = shared_files.lock().unwrap();
    write_usize(chunk, shared_files.len())?;

    for file in shared_files.iter() {
        write_string(chunk, file)?;
    }
    Ok(())
}

// Server impl
fn handle_client(stream: TcpStream, shared_files: SharedFiles) -> io::Result<()> {
    let mut chunk = Chunk::<1024>::new(&stream);

    // Read file_name buffer size
    chunk.run_loop(shared_files, |chunk, shared_files| {
        chunk.read_stream(1)?;
        match u8::from_le_bytes(chunk.to_byte_array::<1>()) {
            0 => add_file(chunk, shared_files)?,
            1 => get_file(chunk)?,
            2 => fetch_files(chunk, shared_files)?,

            // Keep alive
            3 => {}

            n => panic!("Unknown op byte {n}"),
        }

        Ok(())
    })
}

fn load_all_files(shared_files: &mut SharedFiles) {
    let paths = fs::read_dir(SERVER_FILES).unwrap();
    let mut shared_files = shared_files.lock().unwrap();

    paths.for_each(|p| _ = shared_files.insert(p.unwrap().file_name().into_string().unwrap()));
}

fn main() -> io::Result<()> {
    let mut shared_files = Arc::new(Mutex::new(HashSet::new()));

    load_all_files(&mut shared_files);

    let listener = TcpListener::bind(SERVER_ADDR)?;
    let pool = ThreadPool::new(THREAD_COUNT);
    println!("Listening for connections...");

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let files = shared_files.clone();
            pool.execute(move || {
                handle_client(stream, files).unwrap_or_else(|error| {
                    eprintln!("Client Error: {error}");
                })
            });
        } else {
            eprintln!("Connection failed!");
        }
    }

    Ok(())
}
