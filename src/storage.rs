use alloc::vec;
use core::ops::Range;

use defmt::error;
use embassy_embedded_hal::adapter::BlockingAsync;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, once_lock::OnceLock,
};
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::{FlashStorage, FlashStorageError};
use sequential_storage::{
    cache::{Cache, Uncached},
    map::{MapConfig, MapStorage, SerializationError, Value},
};

pub type StorageError = sequential_storage::Error<FlashStorageError>;

/// Keyed by raw `u8` rather than [`StorageKey`] so that items with keys this firmware doesn't
/// know, such as ones written by a newer build, still deserialize.
type Map =
    MapStorage<u8, BlockingAsync<FlashStorage<'static>>, Cache<Uncached, Uncached, Uncached, u8>>;

/// Scratch buffer size for (de)serializing items. It must fit the largest serialized key and
/// value combined, rounded up to the 4 byte flash word size.
const DATA_BUFFER_SIZE: usize = 256;

struct Storage {
    map: Map,
    data_buffer: [u8; DATA_BUFFER_SIZE],
}

static STORAGE: OnceLock<Mutex<CriticalSectionRawMutex, Storage>> = OnceLock::new();

/// Identifies an item in flash storage.
///
/// The value is written to flash, so existing keys must never be renumbered or reused for a
/// different type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
#[repr(u8)]
pub enum StorageKey {
    WifiCredentials = 0,
    MqttConfig = 1,
}

/// Sets up the key-value store on the NVS partition.
///
/// # Panics
///
/// Panics if called more than once, or if the partition table has no NVS partition.
pub fn init(flash: esp_hal::peripherals::FLASH<'static>) {
    let mut flash = FlashStorage::new(flash);
    let range = nvs_range(&mut flash);

    let map = MapStorage::new(
        BlockingAsync::new(flash),
        MapConfig::try_new(range).expect("NVS partition is not usable as a storage map"),
        Cache::new_uncached(),
    );

    if STORAGE
        .init(Mutex::new(Storage {
            map,
            data_buffer: [0; DATA_BUFFER_SIZE],
        }))
        .is_err()
    {
        panic!("storage initialized more than once");
    }
}

fn nvs_range(flash: &mut FlashStorage) -> Range<u32> {
    let mut buffer = vec![0; partitions::PARTITION_TABLE_MAX_LEN];
    let table = partitions::read_partition_table(flash, &mut buffer)
        .expect("failed to read partition table");
    let nvs = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
        .expect("failed to read partition table")
        .expect("partition table has no NVS partition");

    nvs.offset()..nvs.offset() + nvs.len()
}

fn storage() -> &'static Mutex<CriticalSectionRawMutex, Storage> {
    STORAGE.try_get().expect("storage not initialized")
}

/// Returns the value stored under `key`, or `None` if it is absent or can't be read.
pub async fn load<V>(key: StorageKey) -> Option<V>
where
    V: for<'a> Value<'a>,
{
    let mut storage = storage().lock().await;
    let Storage { map, data_buffer } = &mut *storage;

    match map.fetch_item(data_buffer, &(key as u8)).await {
        Ok(value) => value,
        Err(e) => {
            error!("failed to load storage key {}: {}", key, e);
            None
        }
    }
}

/// Stores `value` under `key`, replacing any previous value.
pub async fn store<'a, V: Value<'a>>(key: StorageKey, value: &V) -> Result<(), StorageError> {
    let mut storage = storage().lock().await;
    let Storage { map, data_buffer } = &mut *storage;

    map.store_item(data_buffer, &(key as u8), value).await
}

/// Removes the value stored under `key`. Succeeds if there is no such value.
pub async fn remove(key: StorageKey) -> Result<(), StorageError> {
    let mut storage = storage().lock().await;
    let Storage { map, data_buffer } = &mut *storage;

    map.remove_item(data_buffer, &(key as u8)).await
}

/// Writes `s` as a one byte length followed by its bytes. `s` must be at most 255 bytes.
pub fn serialize_str(s: &str, buffer: &mut [u8]) -> Result<usize, SerializationError> {
    let len = u8::try_from(s.len()).map_err(|_| SerializationError::InvalidData)?;
    let out = buffer
        .get_mut(..1 + s.len())
        .ok_or(SerializationError::BufferTooSmall)?;

    out[0] = len;
    out[1..].copy_from_slice(s.as_bytes());

    Ok(out.len())
}

/// Reads a string written by [`serialize_str`], returning it and the number of bytes it took up.
pub fn deserialize_str<const N: usize>(
    buffer: &[u8],
) -> Result<(heapless::String<N>, usize), SerializationError> {
    let (&len, rest) = buffer
        .split_first()
        .ok_or(SerializationError::InvalidFormat)?;
    let bytes = rest
        .get(..len as usize)
        .ok_or(SerializationError::InvalidFormat)?;
    let s = core::str::from_utf8(bytes).map_err(|_| SerializationError::InvalidFormat)?;
    let s = heapless::String::try_from(s).map_err(|_| SerializationError::InvalidFormat)?;

    Ok((s, 1 + bytes.len()))
}
