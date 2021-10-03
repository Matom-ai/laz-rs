use super::chunk_table::{
    update_chunk_table_offset, write_chunk_table, ChunkTable, ChunkTableEntry,
};
use super::LazVlr;
use crate::byteslice::ChunksIrregularMut;
use byteorder::{LittleEndian, WriteBytesExt};
use std::io::{Read, Seek, SeekFrom, Write};

use super::details::{record_compressor_from_laz_items, record_decompressor_from_laz_items};

fn compress_one_chunk<W: Write + Seek + Send>(
    chunk_data: &[u8],
    vlr: &LazVlr,
    mut dest: &mut W,
) -> std::io::Result<u64> {
    let start = dest.seek(SeekFrom::Current(0))?;
    {
        let mut compressor = record_compressor_from_laz_items(&vlr.items(), &mut dest).unwrap();
        compressor.compress_many(chunk_data)?;
        compressor.done()?;
    }
    let end = dest.seek(SeekFrom::Current(0))?;
    Ok(end - start)
}

/// LasZip compressor that compresses using multiple threads
///
/// This works by forming complete chunks of points with the points
/// data passed when [`compress_many`] is called. These complete chunks are
/// compressed & written right away and points that are 'leftovers' are kept until
/// the next call to [`compress_many`] or [`done`].
///
/// [`compress_many`]: ./struct.ParLasZipCompressor.html#method.compress_many
/// [`done`]: Self::done
#[cfg(feature = "parallel")]
pub struct ParLasZipCompressor<W> {
    vlr: LazVlr,
    /// Size in bytes of each chunks written so far
    chunk_table: Vec<usize>,
    /// offset from beginning of the file to where the
    /// offset to chunk table will be written
    table_offset: i64,
    // Stores uncompressed points from the last call to compress_many
    // that did not allow to make a full chunk of the requested vlr.chunk_size
    // They are prepended to the points data passed to the compress_many fn.
    // The rest is compressed when done is called, forming the last chunk
    rest: Vec<u8>,
    dest: W,
}

#[cfg(feature = "parallel")]
impl<W: Write + Seek + Send> ParLasZipCompressor<W> {
    /// Creates a new ParLasZipCompressor
    pub fn new(dest: W, vlr: LazVlr) -> crate::Result<Self> {
        if vlr.uses_variably_sized_chunks() {
            panic!("Variable sized chunks not supported when writing");
        }
        let rest = Vec::<u8>::with_capacity(vlr.num_bytes_in_decompressed_chunk() as usize);
        Ok(Self {
            vlr,
            chunk_table: vec![],
            table_offset: -1,
            rest,
            dest,
        })
    }

    /// Reserves and prepares the offset to chunk table that will be
    /// updated when [done] is called.
    ///
    /// This method will automatically be called on the first point(s) being compressed,
    /// but for some scenarios, manually calling this might be useful.
    ///
    /// [done]: Self::done
    pub fn reserve_offset_to_chunk_table(&mut self) -> std::io::Result<()> {
        self.table_offset = self.dest.seek(SeekFrom::Current(0))? as i64;
        self.dest.write_i64::<LittleEndian>(self.table_offset)
    }

    /// Compresses many points using multiple threads
    ///
    /// For this function to actually use multiple threads, the `points`
    /// buffer shall hold more points that the vlr's `chunk_size`.
    pub fn compress_many(&mut self, points: &[u8]) -> std::io::Result<()> {
        if self.table_offset == -1 {
            self.reserve_offset_to_chunk_table()?;
        }
        let point_size = self.vlr.items_size() as usize;
        debug_assert_eq!(self.rest.len() % point_size, 0);

        let chunk_size_in_bytes = self.vlr.chunk_size() as usize * point_size;
        debug_assert!(self.rest.len() < chunk_size_in_bytes);
        let mut compressible_buf = points;

        if self.rest.len() != 0 {
            // Try to complete our rest buffer to form a complete chunk
            let missing_bytes = chunk_size_in_bytes - self.rest.len();
            let num_bytes_to_copy = missing_bytes.min(compressible_buf.len());
            self.rest
                .extend_from_slice(&compressible_buf[..num_bytes_to_copy]);

            if self.rest.len() < chunk_size_in_bytes {
                // rest + points did not form a complete chunk,
                // no need to go further.
                return Ok(());
            }

            debug_assert_eq!(self.rest.len(), chunk_size_in_bytes);
            // We have a complete chunk, lets compress it now
            let chunk_size = compress_one_chunk(&self.rest, &self.vlr, &mut self.dest)?;
            self.chunk_table.push(chunk_size as usize);
            self.rest.clear();

            compressible_buf = &compressible_buf[missing_bytes..]
        }
        debug_assert_eq!(compressible_buf.len() % point_size, 0);

        // Copy bytes which does not form a complete chunk into our rest.
        let num_excess_bytes = compressible_buf.len() % chunk_size_in_bytes;
        let (compressible_buf, excess_bytes) =
            compressible_buf.split_at(compressible_buf.len() - num_excess_bytes);
        debug_assert_eq!(excess_bytes.len(), num_excess_bytes);
        if !excess_bytes.is_empty() {
            self.rest.extend_from_slice(excess_bytes);
        }

        if !compressible_buf.is_empty() {
            let chunk_sizes = par_compress(&mut self.dest, compressible_buf, &self.vlr).unwrap();
            chunk_sizes
                .iter()
                .copied()
                .map(|size| size as usize)
                .for_each(|size| self.chunk_table.push(size));
        }

        Ok(())
    }

    /// Tells the compressor that no more points will be compressed
    ///
    /// - Compresses & writes the rest of the points to form the last chunk
    /// - Writes the chunk table
    /// - update the offset to the chunk_table
    pub fn done(&mut self) -> crate::Result<()> {
        if self.rest.len() != 0 {
            debug_assert!(self.rest.len() <= self.vlr.num_bytes_in_decompressed_chunk() as usize);
            let last_chunk_size = compress_one_chunk(&self.rest, &self.vlr, &mut self.dest)?;
            self.chunk_table.push(last_chunk_size as usize);
        }

        if self.table_offset == -1 && self.chunk_table.is_empty() {
            // No call to compress_many was made
            self.reserve_offset_to_chunk_table()?;
        }
        update_chunk_table_offset(&mut self.dest, SeekFrom::Start(self.table_offset as u64))?;
        write_chunk_table(&mut self.dest, &self.chunk_table)?;
        Ok(())
    }

    pub fn vlr(&self) -> &LazVlr {
        &self.vlr
    }

    pub fn into_inner(self) -> W {
        self.dest
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.dest
    }

    pub fn get(&self) -> &W {
        &self.dest
    }
}

impl<W: Write + Seek + Send> super::LazCompressor for ParLasZipCompressor<W> {
    fn compress_many(&mut self, points: &[u8]) -> crate::Result<()> {
        self.compress_many(points)?;
        Ok(())
    }

    fn done(&mut self) -> crate::Result<()> {
        self.done()?;
        Ok(())
    }
}

#[cfg(feature = "parallel")]
/// Laszip decompressor, that can decompress data using multiple threads
pub struct ParLasZipDecompressor<R> {
    vlr: LazVlr,
    chunk_table: ChunkTable,
    last_chunk_read: isize,
    start_of_data: u64,
    // Same idea as in ParLasZipCompressor
    rest: std::io::Cursor<Vec<u8>>,
    // `internal_buffer` is used to hold the chunks to be decompressed
    // after copying them from the source.
    // Each thread will receive a chunk of this buffer to decompress it.
    // It makes the decompression io-free, which means no performance penalty.
    // And it is ok to have such an internal buffer as
    // the compressed data is much much smaller that uncompressed data.
    internal_buffer: Vec<u8>,
    source: R,
}

#[cfg(feature = "parallel")]
impl<R: Read + Seek> ParLasZipDecompressor<R> {
    /// Creates a new decompressor
    ///
    /// Fails if no chunk table could be found.
    pub fn new(mut source: R, vlr: LazVlr) -> crate::Result<Self> {
        let chunk_table = ChunkTable::read_from(&mut source, &vlr)?;
        let start_of_data = source.seek(SeekFrom::Current(0))?;
        let biggest_chunk = chunk_table
            .as_ref()
            .into_iter()
            .map(|entry| entry.point_count)
            .max()
            .unwrap();
        let vec = Vec::<u8>::with_capacity(biggest_chunk as usize);
        let rest = std::io::Cursor::new(vec);

        Ok(Self {
            source,
            vlr,
            chunk_table,
            rest,
            internal_buffer: vec![],
            last_chunk_read: -1,
            start_of_data,
        })
    }

    /// Decompresses many points using multiple threads
    ///
    /// For this function to actually use multiple threads, the `points`
    /// buffer shall hold more points that the vlr's `chunk_size`.
    pub fn decompress_many(&mut self, out: &mut [u8]) -> crate::Result<()> {
        let point_size = self.vlr.items_size() as usize;
        assert_eq!(out.len() % point_size, 0);

        let num_bytes_in_rest = self.rest.get_ref().len() - self.rest.position() as usize;
        debug_assert!(num_bytes_in_rest % point_size == 0);

        let (out_rest, out_decompress) = if num_bytes_in_rest >= out.len() {
            (out, &mut [] as &mut [u8])
        } else {
            out.split_at_mut(num_bytes_in_rest)
        };

        // 1. Copy the data from our rest into the caller's buffer
        if !out_rest.is_empty() {
            self.rest.read(out_rest)?;
        }

        if out_decompress.is_empty() {
            return Ok(());
        }

        debug_assert_eq!(
            self.rest.position() as usize,
            self.rest.get_mut().len(),
            "Rest buffer not completely consumed"
        );
        self.rest.get_mut().clear();
        self.rest.set_position(0);

        // 2. Find out how many chunks we have to decompress
        // and load their compressed bytes in our internal buffer
        let num_requested_points_left = out_decompress.len() / point_size;
        let start_index = (self.last_chunk_read + 1) as usize;
        let mut num_points = 0usize;
        let mut num_chunks_to_decompress = 0;
        let mut num_bytes_to_read = 0usize;
        for entry in &self.chunk_table[start_index..] {
            num_points += entry.point_count as usize;
            num_chunks_to_decompress += 1;
            num_bytes_to_read += entry.byte_count as usize;
            if num_points >= num_requested_points_left {
                break;
            }
        }
        let end_index = start_index + num_chunks_to_decompress;

        debug_assert!(num_chunks_to_decompress >= 1);
        debug_assert!(num_points >= num_requested_points_left);
        // TODO if count < num_requested_points_left then the user ask to decompress more points
        //      than there are

        // Read the necessary compressed bytes into our internal buffer
        self.internal_buffer.resize(num_bytes_to_read, 0u8);
        self.source.read(&mut self.internal_buffer)?;

        // 3. Decompress
        // The idea is that if we have `n` chunks to decompress
        // we decompress `n-1` 'normally', and we do something special for the
        // last chunk as we have to handle the `rest`.
        //
        // We start by splitting all the buffers in two, one for the so called `n-1` chunks
        // and one for the `n` chunk.
        let head_chunks_table = &self.chunk_table[start_index..end_index - 1];
        let tail_chunk_entry = self.chunk_table[end_index - 1];
        let num_bytes_in_head_chunks = num_bytes_to_read - tail_chunk_entry.byte_count as usize;
        let num_points_in_head_chunks = num_points - tail_chunk_entry.point_count as usize;
        let (head_chunks, tail_chunk) = self.internal_buffer.split_at(num_bytes_in_head_chunks);
        let (head_output, tail_output) =
            out_decompress.split_at_mut(num_points_in_head_chunks * point_size);

        // These are to make the borrow checker happy self is not `Send`.
        let rest = &mut self.rest;
        let vlr = &self.vlr;
        let chunk_table_len = self.chunk_table.len();
        let (res1, res2) = rayon::join(
            || -> crate::Result<()> {
                par_decompress(head_chunks, head_output, &vlr, head_chunks_table)
            },
            || -> crate::Result<()> {
                let mut last_src = std::io::Cursor::new(tail_chunk);
                let mut decompressor =
                    record_decompressor_from_laz_items(&vlr.items(), &mut last_src)?;
                // Decompress what we can in the caller's buffer
                decompressor.decompress_many(tail_output)?;
                // Then, decompress what we did not, into our rest buffer
                let num_bytes_left =
                    (tail_chunk_entry.point_count as usize * point_size) - tail_output.len();
                if !vlr.uses_variably_sized_chunks() && end_index == chunk_table_len {
                    // When fixed-size chunks are used, for the last chunk, the number of point
                    // is unknown, so we have to decompress it until an end of file appears
                    rest.get_mut().resize(num_bytes_left, 0u8);
                    let num_actually_decompressed =
                        decompressor.decompress_until_end_of_file(rest.get_mut())?;
                    rest.get_mut().resize(num_actually_decompressed, 0u8);
                } else {
                    rest.get_mut().resize(num_bytes_left, 0u8);
                    decompressor.decompress_many(rest.get_mut())?;
                }
                rest.set_position(0);
                Ok(())
            },
        );
        res1?;
        res2?;

        self.last_chunk_read += num_chunks_to_decompress as isize;
        Ok(())
    }

    /// Seeks to the position of the point at the given index
    pub fn seek(&mut self, index: u64) -> crate::Result<()> {
        // Throw away what's in the rest buffer
        self.rest.set_position(0);
        self.rest.get_mut().clear();

        let chunk_of_point = (index / self.vlr.chunk_size() as u64) as usize;
        if chunk_of_point >= self.chunk_table.len() {
            let _ = self.source.seek(SeekFrom::End(0))?;
            return Ok(());
        }
        // Seek to the start of the points chunk
        // and read the chunk data
        let start_of_chunk_pos = self.start_of_data
            + self.chunk_table[..chunk_of_point]
                .iter()
                .map(|entry| entry.byte_count)
                .sum::<u64>();
        self.source.seek(SeekFrom::Start(start_of_chunk_pos))?;
        self.internal_buffer
            .resize(self.chunk_table[chunk_of_point].byte_count as usize, 0u8);
        self.source.read(&mut self.internal_buffer)?;

        // Completely decompress the chunk
        self.rest
            .get_mut()
            .resize(self.vlr.num_bytes_in_decompressed_chunk() as usize, 0u8);
        let mut decompressor = record_decompressor_from_laz_items(
            self.vlr.items(),
            std::io::Cursor::new(&self.internal_buffer),
        )?;
        let is_last_chunk = chunk_of_point == (self.chunk_table.len() - 1);
        if is_last_chunk {
            let num_bytes_decompressed =
                decompressor.decompress_until_end_of_file(self.rest.get_mut())?;
            let num_points_in_last_chunk = num_bytes_decompressed / self.vlr.items_size() as usize;
            let pos_in_chunk = index % self.vlr.chunk_size() as u64;
            if pos_in_chunk as usize >= num_points_in_last_chunk as usize {
                // Make the rest appear as fully consumed to
                // force EOF error on next decompression
                self.rest.set_position(self.rest.get_ref().len() as u64);
                return Ok(());
            }
        } else {
            decompressor.decompress_many(self.rest.get_mut())?;
        }
        // This effectively discard points that were
        // before the one we just seeked to
        let pos_in_chunk = index % self.vlr.chunk_size() as u64;
        self.rest.set_position(pos_in_chunk * self.vlr.items_size());
        self.last_chunk_read = chunk_of_point as isize;
        Ok(())
    }

    pub fn into_inner(self) -> R {
        self.source
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.source
    }

    pub fn get(&self) -> &R {
        &self.source
    }
}

impl<R: Read + Seek> super::LazDecompressor for ParLasZipDecompressor<R> {
    fn decompress_many(&mut self, points: &mut [u8]) -> crate::Result<()> {
        self.decompress_many(points)
    }

    fn seek(&mut self, index: u64) -> crate::Result<()> {
        self.seek(index)
    }
}

/// Compresses all points in parallel
///
/// Just like [`compress_buffer`] but the compression is done in multiple threads
///
/// # Note
///
/// Point order [is conserved](https://github.com/rayon-rs/rayon/issues/551)
///
/// [`compress_buffer`]: crate::compress_buffer
#[cfg(feature = "parallel")]
pub fn par_compress_buffer<W: Write + Seek>(
    dst: &mut W,
    uncompressed_points: &[u8],
    laz_vlr: &LazVlr,
) -> crate::Result<()> {
    let start_pos = dst.seek(SeekFrom::Current(0))?;
    // Reserve the bytes for the chunk table offset that will be updated later
    dst.write_i64::<LittleEndian>(start_pos as i64)?;

    let chunk_sizes = par_compress(dst, uncompressed_points, laz_vlr)?;

    update_chunk_table_offset(dst, SeekFrom::Start(start_pos))?;
    write_chunk_table(dst, &chunk_sizes)?;
    Ok(())
}

/// Compresses the points contained in `uncompressed_points` writing the result in the `dst`
/// and returns the size of each chunk
///
/// Does not write nor update the offset to the chunk table
/// And does not write the chunk table
///
/// Returns the size of each compressed chunk of point written
#[cfg(feature = "parallel")]
pub fn par_compress<W: Write>(
    dst: &mut W,
    uncompressed_points: &[u8],
    laz_vlr: &LazVlr,
) -> crate::Result<Vec<usize>> {
    use rayon::iter::{IntoParallelIterator, ParallelIterator};
    use std::io::Cursor;
    debug_assert_eq!(uncompressed_points.len() % laz_vlr.items_size() as usize, 0);
    let point_size = laz_vlr.items_size() as usize;
    let points_per_chunk = laz_vlr.chunk_size() as usize;
    let chunk_size_in_bytes = points_per_chunk * point_size;

    // The last chunk may not have the same size,
    // the chunks() method takes care of that for us
    let all_slices = uncompressed_points
        .chunks(chunk_size_in_bytes)
        .collect::<Vec<_>>();

    let chunks = all_slices
        .into_par_iter()
        .map(|slc| {
            let mut record_compressor =
                record_compressor_from_laz_items(&laz_vlr.items(), Cursor::new(Vec::<u8>::new()))?;

            for raw_point in slc.chunks_exact(point_size) {
                record_compressor.compress_next(raw_point)?;
            }
            record_compressor.done()?;

            Ok(record_compressor.box_into_inner())
        })
        .collect::<Vec<crate::Result<Cursor<Vec<u8>>>>>();

    let mut chunk_sizes = Vec::<usize>::with_capacity(chunks.len());
    for chunk_result in chunks {
        let chunk = chunk_result?;
        chunk_sizes.push(chunk.get_ref().len());
        dst.write_all(chunk.get_ref())?;
    }
    Ok(chunk_sizes)
}

/// Decompresses all points from the buffer in parallel.
///
/// Each chunk is sent for decompression in a thread.
///
/// Just like [`decompress_buffer`] but the decompression is done using multiple threads
///
/// # Important
///
/// All the points in the doc of [`decompress_buffer`] applies to this
/// fn with the addition that  the chunk table _IS_ mandatory
///
/// [`decompress_buffer`]: fn.decompress_buffer.html
#[cfg(feature = "parallel")]
pub fn par_decompress_buffer(
    compressed_points_data: &[u8],
    decompressed_points: &mut [u8],
    laz_vlr: &LazVlr,
) -> crate::Result<()> {
    debug_assert_eq!(decompressed_points.len() % laz_vlr.items_size() as usize, 0);
    let mut cursor = std::io::Cursor::new(compressed_points_data);
    let chunk_table = ChunkTable::read_from(&mut cursor, &laz_vlr)?;

    let num_point_bytes = chunk_table
        .as_ref()
        .iter()
        .map(|entry| entry.byte_count as usize)
        .sum::<usize>();

    let compressed_points = &compressed_points_data[std::mem::size_of::<i64>()..num_point_bytes];
    par_decompress(
        compressed_points,
        decompressed_points,
        laz_vlr,
        chunk_table.as_ref(),
    )
}

/// Actual the parallel decompression
///
/// `compressed_points` must contains only the bytes corresponding to the points
/// (so no offset, no chunk_table)
#[cfg(feature = "parallel")]
fn par_decompress(
    compressed_points: &[u8],
    decompressed_points: &mut [u8],
    laz_vlr: &LazVlr,
    chunk_table: &[ChunkTableEntry],
) -> crate::Result<()> {
    use crate::byteslice::ChunksIrregular;
    use rayon::iter::{IntoParallelIterator, ParallelIterator};
    let sizes = chunk_table.iter().map(|entry| entry.byte_count as usize);
    let counts = chunk_table
        .iter()
        .map(|entry| (entry.point_count * laz_vlr.items_size()) as usize);
    let input_chunks_iter = ChunksIrregular::new(compressed_points, sizes);
    let output_chunks_iter = ChunksIrregularMut::new(decompressed_points, counts);

    // FIXME we collect into a Vec because zip cannot be made 'into_par_iter' by rayon
    //  (or at least i don't know how)
    let decompression_jobs: Vec<(&[u8], &mut [u8])> =
        input_chunks_iter.zip(output_chunks_iter).collect();
    decompression_jobs
        .into_par_iter()
        .map(|(chunk_in, chunk_out)| {
            let src = std::io::Cursor::new(chunk_in);
            let mut record_decompressor = record_decompressor_from_laz_items(laz_vlr.items(), src)?;
            record_decompressor.decompress_many(chunk_out)?;
            Ok(())
        })
        .collect::<crate::Result<()>>()?;
    Ok(())
}

/// Decompress points from the file in parallel greedily
///
/// What is meant by 'greedy' here is that this function
/// will read in memory all the compressed points in order to decompress them
/// as opposed to reading a chunk of points when needed
///
/// This fn will decompress as many points as the `decompress_points` can hold.
/// (But will still load the whole point data in memory even if the
/// `decompress_points` cannot hold all the points), meaning that points that could
/// not fit in the `points_out` buffer will be lost.
///
/// Each chunk is sent for decompression in a thread.
///
///
/// `src` must be at the start of the LAZ point data
#[cfg(feature = "parallel")]
pub fn par_decompress_all_from_file_greedy(
    mut src: &mut std::io::BufReader<std::fs::File>,
    points_out: &mut [u8],
    laz_vlr: &LazVlr,
) -> crate::Result<()> {
    debug_assert_eq!(points_out.len() % laz_vlr.items_size() as usize, 0);
    let chunk_table = ChunkTable::read_from(&mut src, &laz_vlr)?;
    let num_point_bytes = chunk_table
        .as_ref()
        .iter()
        .map(|entry| entry.byte_count as usize)
        .sum::<usize>();

    let mut compressed_points = vec![0u8; num_point_bytes as usize];
    src.read_exact(&mut compressed_points)?;
    par_decompress(
        &compressed_points,
        points_out,
        laz_vlr,
        chunk_table.as_ref(),
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{LazItemRecordBuilder, LazItemType};

    #[cfg(feature = "parallel")]
    #[test]
    fn test_table_offset_one_point() {
        // Test that if we compress just one point using the Parallel compressor
        // the chunk table offset is correctly reserved
        let vlr = super::LazVlr::from_laz_items(
            LazItemRecordBuilder::new()
                .add_item(LazItemType::Point10)
                .build(),
        );

        let point = vec![0u8; vlr.items_size() as usize];
        let mut compressor =
            ParLasZipCompressor::new(std::io::Cursor::new(Vec::<u8>::new()), vlr).unwrap();
        assert_eq!(compressor.table_offset, -1);
        compressor.compress_many(&point).unwrap();
        assert_eq!(compressor.table_offset, 0);
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn test_table_offset_complete_chunk() {
        // Test that if we compress at least a chunk using the Parallel compressor
        // the chunk table offset is correctly reserved
        let vlr = super::LazVlr::from_laz_items(
            LazItemRecordBuilder::new()
                .add_item(LazItemType::Point10)
                .build(),
        );

        let points = vec![0u8; vlr.num_bytes_in_decompressed_chunk() as usize];
        let mut compressor =
            ParLasZipCompressor::new(std::io::Cursor::new(Vec::<u8>::new()), vlr).unwrap();
        assert_eq!(compressor.table_offset, -1);
        compressor.compress_many(&points).unwrap();
        assert_eq!(compressor.table_offset, 0);
    }
}
