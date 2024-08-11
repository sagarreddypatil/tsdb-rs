use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::{fd::AsRawFd, raw::c_void},
};

use libc::{
    fallocate, madvise, mmap, msync, munmap, MADV_DONTNEED, MADV_RANDOM, MADV_SEQUENTIAL,
    MADV_WILLNEED, MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE,
};

const MAGIC: [u8; 8] = *b"FMAPVEC\0";
const MMAP_SIZE: usize = 1 << 40; // 1 TiB

#[repr(C)]
struct FMVHeader {
    magic: [u8; 8],
    size: usize,

    // header should span a page (4K)
    reserved: [u8; 4096 - 8 - size_of::<usize>()],
}

struct FileMappedVector<T: Copy> {
    file: File,
    capacity: usize,

    header: *mut FMVHeader,
    data: *mut T,
}

impl<T: Copy> FileMappedVector<T> {
    pub fn new(mut file: File) -> anyhow::Result<Self> {
        assert!(size_of::<FMVHeader>() == 4096);

        if file.metadata().unwrap().permissions().readonly() {
            return Err(anyhow::anyhow!("file is not readable and writable"));
        }

        let init_cap = size_of::<T>() * 32;
        let initial_file_size = size_of::<FMVHeader>() + init_cap;

        // open file, initialize if new file
        let fsize = file.metadata().unwrap().len();
        if fsize == 0 {
            file.set_len(initial_file_size as u64).unwrap();

            // initialize header
            let header = FMVHeader {
                magic: MAGIC,
                size: 0,
                reserved: [0; 4096 - 8 - size_of::<usize>()],
            };

            // write header to file
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(unsafe {
                std::slice::from_raw_parts(
                    &header as *const FMVHeader as *const u8,
                    size_of::<FMVHeader>(),
                )
            })
            .unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
        }

        // do some checks
        let fsize = file.metadata().unwrap().len() as usize;
        assert!(fsize >= size_of::<FMVHeader>());

        let fsize = fsize - size_of::<FMVHeader>();
        assert!(fsize % size_of::<T>() == 0);
        let capacity = fsize / size_of::<T>();

        // mmap file
        let header = unsafe {
            mmap(
                std::ptr::null_mut(),
                MMAP_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };

        if (header as isize) == -1 {
            unsafe { libc::perror("mmap\0".as_ptr() as *const i8) };
            return Err(anyhow::anyhow!("mmap failed"));
        }

        let header = header as *mut FMVHeader;
        assert!(header != std::ptr::null_mut());

        let data = unsafe { (header as *const u8).add(size_of::<FMVHeader>()) } as *mut T;

        // madvise
        unsafe { madvise(header as *mut c_void, MMAP_SIZE, MADV_DONTNEED) };
        unsafe {
            madvise(
                data as *mut c_void,
                capacity * size_of::<T>(),
                MADV_SEQUENTIAL,
            )
        };
        unsafe { madvise(header as *mut c_void, size_of::<T>(), MADV_SEQUENTIAL) };

        Ok(Self {
            file,
            capacity,
            header,
            data,
        })
    }

    pub fn push(&mut self, value: T) {
        assert!(!self.header.is_null());
        assert!(!self.data.is_null());

        let header: &mut FMVHeader = unsafe { self.header.as_mut().unwrap() };
        assert!(header.magic == MAGIC);

        if header.size >= self.capacity {
            let new_cap = self.capacity * 2;
            let new_size = size_of::<T>() * new_cap + size_of::<FMVHeader>();
            let old_size = size_of::<T>() * self.capacity + size_of::<FMVHeader>();

            // self.file.set_len(new_size as u64).unwrap();
            let ret = unsafe {
                fallocate(
                    self.file.as_raw_fd(),
                    0,
                    old_size as i64,
                    new_size as i64 - old_size as i64,
                )
            };
            if ret != 0 {
                unsafe { libc::perror("fallocate\0".as_ptr() as *const i8) };
            }

            unsafe {
                madvise(self.data as *mut c_void, old_size, MADV_RANDOM);
                madvise(
                    self.data.add(old_size) as *mut c_void,
                    new_size - old_size,
                    MADV_SEQUENTIAL,
                );
                madvise(
                    self.header as *mut c_void,
                    size_of::<FMVHeader>(),
                    MADV_WILLNEED,
                );
            }

            self.capacity = new_cap;
        }

        unsafe {
            let write_ptr = self.data.add(header.size);
            write_ptr.write(value);

            header.size += 1;
        }
    }

    pub fn len(&self) -> usize {
        unsafe { self.header.as_ref().unwrap().size }
    }
}

impl<T: Copy> std::ops::Index<usize> for FileMappedVector<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        assert!(index < self.capacity);
        unsafe { self.data.add(index).as_ref().unwrap() }
    }
}

impl<T: Copy> Drop for FileMappedVector<T> {
    fn drop(&mut self) {
        unsafe {
            msync(self.header as *mut c_void, MMAP_SIZE, MS_SYNC);
            munmap(self.header as *mut c_void, MMAP_SIZE);
        }

        self.file.sync_all().unwrap();
    }
}

fn main() {
    File::create("fmapvec").unwrap();
    let file = File::options().read(true).write(true).open("fmapvec").unwrap();
    let mut vec = FileMappedVector::<u64>::new(file).unwrap();

    for i in 0..10000 {
        vec.push(i);
    }

    drop(vec);

    let file = File::options().read(true).write(true).open("fmapvec").unwrap();
    let vec = FileMappedVector::<u64>::new(file).unwrap();

    for i in 0..vec.len() {
        println!("{}", vec[i]);
    }
}
