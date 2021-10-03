use crate::record::{
    LayeredPointRecordCompressor, LayeredPointRecordDecompressor, RecordCompressor,
    RecordDecompressor, SequentialPointRecordCompressor, SequentialPointRecordDecompressor,
};
use crate::{LasZipError, LazItem};
use std::io::{Read, Seek, Write};

pub(super) fn record_decompressor_from_laz_items<'a, R: Read + Seek + Send + 'a>(
    items: &Vec<LazItem>,
    input: R,
) -> crate::Result<Box<dyn RecordDecompressor<R> + Send + 'a>> {
    let first_item = items
        .get(0)
        .expect("There should be at least one LazItem to be able to create a RecordDecompressor");

    let mut decompressor = match first_item.version {
        1 | 2 => {
            let decompressor = SequentialPointRecordDecompressor::new(input);
            Box::new(decompressor) as Box<dyn RecordDecompressor<R> + Send>
        }
        3 | 4 => {
            let decompressor = LayeredPointRecordDecompressor::new(input);
            Box::new(decompressor) as Box<dyn RecordDecompressor<R> + Send>
        }
        _ => {
            return Err(LasZipError::UnsupportedLazItemVersion(
                first_item.item_type,
                first_item.version,
            ));
        }
    };

    decompressor.set_fields_from(items)?;
    Ok(decompressor)
}

pub(super) fn record_compressor_from_laz_items<'a, W: Write + Send + 'a>(
    items: &Vec<LazItem>,
    output: W,
) -> crate::Result<Box<dyn RecordCompressor<W> + Send + 'a>> {
    let first_item = items
        .get(0)
        .expect("There should be at least one LazItem to be able to create a RecordCompressor");

    let mut compressor = match first_item.version {
        1 | 2 => {
            let compressor = SequentialPointRecordCompressor::new(output);
            Box::new(compressor) as Box<dyn RecordCompressor<W> + Send>
        }
        3 | 4 => {
            let compressor = LayeredPointRecordCompressor::new(output);
            Box::new(compressor) as Box<dyn RecordCompressor<W> + Send>
        }
        _ => {
            return Err(LasZipError::UnsupportedLazItemVersion(
                first_item.item_type,
                first_item.version,
            ));
        }
    };
    compressor.set_fields_from(items)?;
    Ok(compressor)
}
