use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VirtioMemError {
    #[error("virtio-mem device size must be greater than zero")]
    ZeroSize,
    #[error("virtio-mem block size {actual} is smaller than the minimum {minimum} bytes")]
    BlockSizeTooSmall { actual: u64, minimum: u64 },
    #[error("virtio-mem block size must be a power of two: {0} bytes")]
    BlockSizeNotPowerOfTwo(u64),
    #[error("virtio-mem device size {size} is not a multiple of block size {block}")]
    SizeNotAligned { size: u64, block: u64 },
    #[error("virtio-mem {name} value {value} is outside device size {size}")]
    ValueOutsideSize {
        name: &'static str,
        value: u64,
        size: u64,
    },
    #[error("virtio-mem {name} value {value} is not aligned to block size {block}")]
    ValueNotAligned {
        name: &'static str,
        value: u64,
        block: u64,
    },
}
