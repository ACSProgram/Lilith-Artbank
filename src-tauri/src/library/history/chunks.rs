use rusqlite::{Connection, Transaction, params};
use serde::de::DeserializeOwned;

use crate::app::error::{AppError, AppResult};

use super::{Chunk, ChunkFormat};

pub(super) fn load_chunks(connection: &Connection, operation_id: i64) -> AppResult<Vec<Chunk>> {
    let mut statement = connection.prepare(
        "SELECT format, data FROM operation_chunks WHERE operation_id = ?1 ORDER BY seq",
    )?;
    let rows = statement
        .query_map([operation_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(format, data)| {
            Ok(Chunk {
                format: ChunkFormat::parse(&format)?,
                data,
            })
        })
        .collect()
}

pub(super) fn decode_triples(bytes: &[u8]) -> AppResult<Vec<(i64, i64, i64)>> {
    if !bytes.len().is_multiple_of(24) {
        return Err(AppError::message("记录数据长度无效"));
    }
    Ok(bytes
        .chunks_exact(24)
        .map(|slot| {
            (
                i64::from_le_bytes(slot[0..8].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[8..16].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[16..24].try_into().expect("8 字节")),
            )
        })
        .collect())
}

pub(super) fn decode_moves(bytes: &[u8]) -> AppResult<Vec<(i64, i64, i64, i64)>> {
    if !bytes.len().is_multiple_of(32) {
        return Err(AppError::message("记录数据长度无效"));
    }
    Ok(bytes
        .chunks_exact(32)
        .map(|slot| {
            (
                i64::from_le_bytes(slot[0..8].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[8..16].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[16..24].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[24..32].try_into().expect("8 字节")),
            )
        })
        .collect())
}

pub(super) fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> AppResult<T> {
    Ok(serde_json::from_slice(bytes)?)
}

pub(super) fn insert_chunks(
    transaction: &Transaction<'_>,
    operation_id: i64,
    chunks: &[Chunk],
) -> AppResult<()> {
    for (seq, chunk) in chunks.iter().enumerate() {
        transaction.execute(
            "INSERT INTO operation_chunks(operation_id, seq, format, data)
             VALUES (?1, ?2, ?3, ?4)",
            params![operation_id, seq as i64, chunk.format.as_str(), chunk.data],
        )?;
    }
    Ok(())
}
