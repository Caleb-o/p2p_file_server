use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Write},
    net::TcpStream,
    path::Path,
    sync::{mpsc, Arc, Mutex},
    thread,
};

pub const SERVER_ADDR: &'static str = "192.168.0.148:8000";

pub type SharedFiles = Arc<Mutex<HashSet<String>>>;

pub struct Chunk<'a, const N: usize> {
    stream: &'a TcpStream,
    buffer: [u8; N],
    bytes_sent: usize,
    last_insert: usize,
}

impl<'a, const N: usize> Chunk<'a, N> {
    pub fn new(stream: &'a TcpStream) -> Self {
        Self {
            stream,
            buffer: [0u8; N],
            bytes_sent: 0,
            last_insert: 0,
        }
    }

    pub fn run_loop(
        &mut self,
        shared_files: SharedFiles,
        f: impl Fn(&mut Self, SharedFiles) -> io::Result<()>,
    ) -> io::Result<()> {
        loop {
            f(self, shared_files.clone())?;
        }
    }

    #[inline]
    pub fn sent(&self) -> usize {
        self.bytes_sent
    }

    #[inline]
    pub const fn len(&self) -> usize {
        N
    }

    #[inline]
    pub fn reset(&mut self) {
        self.bytes_sent = 0;
        self.last_insert = 0;
    }

    #[inline]
    pub fn slice(&self, count: usize) -> &[u8] {
        &self.buffer[..count]
    }

    #[inline]
    pub fn slice_mut(&mut self, count: usize) -> &mut [u8] {
        &mut self.buffer[..count]
    }

    pub fn to_byte_array<const S: usize>(&self) -> [u8; S] {
        assert!(S <= N);
        self.buffer[..S]
            .try_into()
            .expect("Cannot convert buffer to array")
    }

    pub fn read(&mut self, count: usize) -> io::Result<usize> {
        let bytes_read = self.stream.read(&mut self.buffer[..count])?;
        self.last_insert = count;
        Ok(bytes_read)
    }

    pub fn read_stream(&mut self, count: usize) -> io::Result<()> {
        self.stream.read_exact(&mut self.buffer[..count])?;
        self.last_insert = count;
        Ok(())
    }

    pub fn write_to_buf(&mut self, items: &[u8]) -> usize {
        let bytes_to_write = std::cmp::min(self.buffer.len(), items.len());
        self.buffer[..bytes_to_write].copy_from_slice(items);
        self.last_insert = bytes_to_write;

        bytes_to_write
    }

    pub fn write_and_send(&mut self, items: &[u8]) -> io::Result<()> {
        _ = self.write_to_buf(items);
        self.send_last_write()
    }

    pub fn send(&mut self, count: usize) -> io::Result<()> {
        self.stream.write_all(&self.buffer[..count])?;
        self.bytes_sent += count;
        Ok(())
    }

    pub fn send_last_write(&mut self) -> io::Result<()> {
        self.stream.write_all(&self.buffer[..self.last_insert])?;
        self.bytes_sent += self.last_insert;
        Ok(())
    }
}

#[inline]
pub fn write_usize<const N: usize>(chunk: &mut Chunk<N>, value: usize) -> io::Result<()> {
    chunk.write_and_send(&value.to_le_bytes())
}

pub fn read_usize<const N: usize>(chunk: &mut Chunk<N>) -> usize {
    chunk
        .read_stream(8)
        .expect("Could not read string size bytes");
    usize::from_le_bytes(chunk.to_byte_array::<8>())
}

pub fn write_string<const N: usize>(chunk: &mut Chunk<N>, str: &str) -> io::Result<()> {
    chunk.write_and_send(&str.as_bytes().len().to_le_bytes())?;
    chunk.write_and_send(str.as_bytes())
}

pub fn read_string<const N: usize>(chunk: &mut Chunk<N>) -> io::Result<String> {
    let file_name_count = read_usize(chunk);

    if file_name_count == 0 {
        return Ok(String::new());
    }

    chunk.read_stream(file_name_count)?;
    Ok(String::from_utf8_lossy(chunk.slice(file_name_count)).to_string())
}

pub fn read_bytes<const N: usize>(chunk: &mut Chunk<N>) -> io::Result<Option<Vec<u8>>> {
    let byte_count = read_usize(chunk);

    if byte_count == 0 {
        return Ok(None);
    }

    chunk.read_stream(byte_count)?;
    Ok(Some(Vec::from(chunk.slice(byte_count))))
}

pub fn send_file<const N: usize>(chunk: &mut Chunk<N>, file_name: &str) -> io::Result<()> {
    if !Path::new(file_name).exists() {
        write_usize(chunk, 0)?;
        return Ok(());
    }

    let mut file = fs::File::open(file_name)?;
    let file_size = file.metadata()?.len() as usize;

    // Send file_size to server
    write_usize(chunk, file_size)?;

    chunk.reset();

    // Send file data in chunks
    while chunk.sent() < file_size {
        let bytes_to_read = std::cmp::min(chunk.len(), file_size - chunk.sent());
        let bytes_read = file.read(chunk.slice_mut(bytes_to_read))?;
        chunk.send(bytes_read)?;
    }

    Ok(())
}

pub fn receive_file<const N: usize>(
    chunk: &mut Chunk<N>,
    file_size: usize,
) -> io::Result<Option<Vec<u8>>> {
    if file_size == 0 {
        return Ok(None);
    }

    let mut buffer = Vec::new();
    let mut bytes_received = 0;

    chunk.reset();

    while bytes_received < file_size {
        let bytes_to_read = std::cmp::min(chunk.len(), file_size - bytes_received);
        let bytes_read = chunk.read(bytes_to_read)?;

        buffer.extend(chunk.slice(bytes_to_read));
        bytes_received += bytes_read;
    }

    Ok(Some(buffer))
}

pub struct ThreadPool {
    workers: Vec<Worker>,
    sender: Option<mpsc::Sender<Job>>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

pub enum PoolCreationError {
    NotEnoughThreads,
}

impl ThreadPool {
    /// Create a new ThreadPool.
    ///
    /// The size is the number of threads in the pool.
    ///
    /// # Panics
    ///
    /// The `new` function will panic if the size is zero.
    pub fn new(size: usize) -> Self {
        assert!(size > 0);

        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));

        let mut workers = Vec::with_capacity(size);

        for id in 0..size {
            workers.push(Worker::new(id, Arc::clone(&receiver)));
        }

        Self {
            workers,
            sender: Some(sender),
        }
    }

    pub fn build(size: usize) -> Result<Self, PoolCreationError> {
        if size == 0 {
            return Err(PoolCreationError::NotEnoughThreads);
        }
        Ok(Self::new(size))
    }

    pub fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let job = Box::new(f);
        self.sender.as_ref().unwrap().send(job).unwrap();
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        drop(self.sender.take());

        for worker in &mut self.workers {
            println!("Shutting down worker {}", worker.id);

            if let Some(thread) = worker.thread.take() {
                thread.join().unwrap();
            }
        }
    }
}

struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Job>>>) -> Self {
        let thread = thread::spawn(move || loop {
            let message = receiver.lock().unwrap().recv();

            match message {
                Ok(job) => {
                    println!("Worker {id} got a job; executing.");
                    job();
                }

                Err(_) => {
                    println!("Worker {id} disconnected; shutting down.");
                    break;
                }
            }
        });

        Self {
            id,
            thread: Some(thread),
        }
    }
}
