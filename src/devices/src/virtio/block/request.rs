// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::convert::From;
use std::io::{self, Seek, SeekFrom, Write};
use std::result;

use logger::{IncMetric, METRICS};
use virtio_gen::virtio_blk::*;
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryError, GuestMemoryMmap};

use super::super::DescriptorChain;
use super::device::DiskProperties;
use super::{Error, SECTOR_SHIFT, SECTOR_SIZE};

#[derive(Debug)]
pub enum IoErrStatus {
    BadRequest(Error),
    Flush(io::Error),
    // Read(num_used_bytes, GuestMemoryError)
    Read(u32, GuestMemoryError),
    Seek(io::Error),
    SyncAll(io::Error),
    Write(GuestMemoryError),
}

#[derive(Debug)]
pub enum ErrStatus {
    IoErr(IoErrStatus),
    Unsupported(u32),
}

#[derive(Debug)]
pub enum Status {
    // Ok(num_used_bytes)
    Ok(u32),
    Err(ErrStatus),
}

impl Status {
    pub fn from_result(result: result::Result<u32, ErrStatus>) -> Status {
        match result {
            Ok(status) => Status::Ok(status),
            Err(status) => Status::Err(status),
        }
    }

    pub fn virtio_blk_status(&self) -> u8 {
        let virtio_blk_status = match self {
            Status::Ok(_) => VIRTIO_BLK_S_OK,
            Status::Err(status) => match status {
                ErrStatus::IoErr(_) => VIRTIO_BLK_S_IOERR,
                ErrStatus::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
            },
        };

        virtio_blk_status as u8
    }

    pub fn num_used_bytes(&self) -> u32 {
        let num_used_bytes = match self {
            Status::Ok(num_used_bytes) => *num_used_bytes,
            Status::Err(status) => match status {
                ErrStatus::IoErr(io_err) => match io_err {
                    IoErrStatus::Read(num_used_bytes, _) => *num_used_bytes,
                    _ => 0,
                },
                ErrStatus::Unsupported(_) => 0,
            },
        };
        // account for the status byte
        num_used_bytes + 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RequestType {
    In,
    Out,
    Flush,
    GetDeviceID,
    Unsupported(u32),
}

impl From<u32> for RequestType {
    fn from(value: u32) -> Self {
        match value {
            VIRTIO_BLK_T_IN => RequestType::In,
            VIRTIO_BLK_T_OUT => RequestType::Out,
            VIRTIO_BLK_T_FLUSH => RequestType::Flush,
            VIRTIO_BLK_T_GET_ID => RequestType::GetDeviceID,
            t => RequestType::Unsupported(t),
        }
    }
}

#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct Request {
    pub request_type: RequestType,
    pub data_len: u32,
    pub status_addr: GuestAddress,
    sector: u64,
    data_addr: GuestAddress,
}

/// The request header represents the mandatory fields of each block device request.
///
/// A request header contains the following fields:
///   * request_type: an u32 value mapping to a read, write or flush operation.
///   * reserved: 32 bits are reserved for future extensions of the Virtio Spec.
///   * sector: an u64 value representing the offset where a read/write is to occur.
///
/// The header simplifies reading the request from memory as all request follow
/// the same memory layout.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}

// Safe because RequestHeader only contains plain data.
unsafe impl ByteValued for RequestHeader {}

impl RequestHeader {
    pub fn new(request_type: u32, sector: u64) -> RequestHeader {
        RequestHeader {
            request_type,
            _reserved: 0,
            sector,
        }
    }
    /// Reads the request header from GuestMemoryMmap starting at `addr`.
    ///
    /// Virtio 1.0 specifies that the data is transmitted by the driver in little-endian
    /// format. Firecracker currently runs only on little endian platforms so we don't
    /// need to do an explicit little endian read as all reads are little endian by default.
    /// When running on a big endian platform, this code should not compile, and support
    /// for explicit little endian reads is required.
    #[cfg(target_endian = "little")]
    fn read_from(memory: &GuestMemoryMmap, addr: GuestAddress) -> result::Result<Self, Error> {
        let request_header: RequestHeader = memory.read_obj(addr).map_err(Error::GuestMemory)?;
        Ok(request_header)
    }
}

impl Request {
    pub fn parse(
        avail_desc: &DescriptorChain,
        mem: &GuestMemoryMmap,
    ) -> result::Result<Request, Error> {
        // The head contains the request type which MUST be readable.
        if avail_desc.is_write_only() {
            return Err(Error::UnexpectedWriteOnlyDescriptor);
        }

        let request_header = RequestHeader::read_from(mem, avail_desc.addr)?;
        let mut req = Request {
            request_type: RequestType::from(request_header.request_type),
            sector: request_header.sector,
            data_addr: GuestAddress(0),
            data_len: 0,
            status_addr: GuestAddress(0),
        };

        let data_desc;
        let status_desc;
        let desc = avail_desc
            .next_descriptor()
            .ok_or(Error::DescriptorChainTooShort)?;

        if !desc.has_next() {
            status_desc = desc;
            // Only flush requests are allowed to skip the data descriptor.
            if req.request_type != RequestType::Flush {
                return Err(Error::DescriptorChainTooShort);
            }
        } else {
            data_desc = desc;
            status_desc = data_desc
                .next_descriptor()
                .ok_or(Error::DescriptorChainTooShort)?;

            if data_desc.is_write_only() && req.request_type == RequestType::Out {
                return Err(Error::UnexpectedWriteOnlyDescriptor);
            }
            if !data_desc.is_write_only() && req.request_type == RequestType::In {
                return Err(Error::UnexpectedReadOnlyDescriptor);
            }
            if !data_desc.is_write_only() && req.request_type == RequestType::GetDeviceID {
                return Err(Error::UnexpectedReadOnlyDescriptor);
            }

            req.data_addr = data_desc.addr;
            req.data_len = data_desc.len;
        }

        // The status MUST always be writable.
        if !status_desc.is_write_only() {
            return Err(Error::UnexpectedReadOnlyDescriptor);
        }

        if status_desc.len < 1 {
            return Err(Error::DescriptorLengthTooSmall);
        }

        req.status_addr = status_desc.addr;

        Ok(req)
    }

    fn execute_seek(&self, disk: &mut DiskProperties) -> result::Result<(), ErrStatus> {
        // TODO: perform this logic at request parsing level in the future.
        // Check that the data length is a multiple of 512 as specified in the virtio standard.
        if u64::from(self.data_len) % SECTOR_SIZE != 0 {
            return Err(ErrStatus::IoErr(IoErrStatus::BadRequest(
                Error::InvalidDataLength,
            )));
        }
        let top_sector = self
            .sector
            .checked_add(u64::from(self.data_len) >> SECTOR_SHIFT)
            .ok_or(ErrStatus::IoErr(IoErrStatus::BadRequest(
                Error::InvalidOffset,
            )))?;
        if top_sector > disk.nsectors() {
            return Err(ErrStatus::IoErr(IoErrStatus::BadRequest(
                Error::InvalidOffset,
            )));
        }

        disk.file_mut()
            .seek(SeekFrom::Start(self.sector << SECTOR_SHIFT))
            .map_err(|e| ErrStatus::IoErr(IoErrStatus::Seek(e)))?;

        Ok(())
    }

    pub(crate) fn execute(
        &self,
        disk: &mut DiskProperties,
        mem: &GuestMemoryMmap,
    ) -> result::Result<u32, ErrStatus> {
        match self.request_type {
            RequestType::In => {
                self.execute_seek(disk)?;
                mem.read_exact_from(self.data_addr, disk.file_mut(), self.data_len as usize)
                    .map(|_| {
                        METRICS.block.read_bytes.add(self.data_len as usize);
                        METRICS.block.read_count.inc();
                        self.data_len
                    })
                    .map_err(|e| {
                        let mut num_used_bytes = self.data_len;
                        if let GuestMemoryError::PartialBuffer { completed, .. } = e {
                            METRICS.block.read_bytes.add(completed);
                            // It's safe to cast to u32 since completed < data_len.
                            num_used_bytes = completed as u32;
                        }
                        ErrStatus::IoErr(IoErrStatus::Read(num_used_bytes, e))
                    })
            }
            RequestType::Out => {
                self.execute_seek(disk)?;
                mem.write_all_to(self.data_addr, disk.file_mut(), self.data_len as usize)
                    .map(|_| {
                        METRICS.block.write_bytes.add(self.data_len as usize);
                        METRICS.block.write_count.inc();
                        0
                    })
                    .map_err(|e| {
                        if let GuestMemoryError::PartialBuffer { completed, .. } = e {
                            METRICS.block.write_bytes.add(completed);
                        }
                        ErrStatus::IoErr(IoErrStatus::Write(e))
                    })
            }
            RequestType::Flush => {
                // flush() first to force any cached data out.
                disk.file_mut()
                    .flush()
                    .map_err(|e| ErrStatus::IoErr(IoErrStatus::Flush(e)))?;
                // Sync data out to physical media on host.
                disk.file_mut()
                    .sync_all()
                    .map_err(|e| ErrStatus::IoErr(IoErrStatus::SyncAll(e)))?;
                METRICS.block.flush_count.inc();
                Ok(0)
            }
            RequestType::GetDeviceID => {
                let disk_id = disk.image_id();
                if (self.data_len as usize) < disk_id.len() {
                    return Err(ErrStatus::IoErr(IoErrStatus::BadRequest(
                        Error::InvalidOffset,
                    )));
                }
                mem.write_slice(disk_id, self.data_addr)
                    .map(|_| VIRTIO_BLK_ID_BYTES)
                    .map_err(|e| ErrStatus::IoErr(IoErrStatus::Write(e)))
            }
            RequestType::Unsupported(op) => Err(ErrStatus::Unsupported(op)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::virtio::queue::tests::*;
    use crate::virtio::test_utils::VirtQueue;
    use vm_memory::{Address, GuestAddress, GuestMemory};

    #[test]
    fn test_read_request_header() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let addr = GuestAddress(0);
        let sector = 123_454_321;

        // Test that all supported request types are read correctly from memory.
        let supported_request_types = vec![
            VIRTIO_BLK_T_IN,
            VIRTIO_BLK_T_OUT,
            VIRTIO_BLK_T_FLUSH,
            VIRTIO_BLK_T_GET_ID,
        ];

        for request_type in supported_request_types {
            let expected_header = RequestHeader::new(request_type, sector);
            mem.write_obj::<RequestHeader>(expected_header, addr)
                .unwrap();

            let actual_header = RequestHeader::read_from(&mem, addr).unwrap();
            assert_eq!(actual_header.request_type, expected_header.request_type);
            assert_eq!(actual_header.sector, expected_header.sector);
        }

        // Test that trying to read a request header that goes outside of the
        // memory boundary fails.
        assert!(RequestHeader::read_from(&mem, GuestAddress(0x1000)).is_err());
    }

    #[test]
    fn test_request_type_from() {
        assert_eq!(RequestType::from(VIRTIO_BLK_T_IN), RequestType::In);
        assert_eq!(RequestType::from(VIRTIO_BLK_T_OUT), RequestType::Out);
        assert_eq!(RequestType::from(VIRTIO_BLK_T_FLUSH), RequestType::Flush);
        assert_eq!(
            RequestType::from(VIRTIO_BLK_T_GET_ID),
            RequestType::GetDeviceID
        );
        assert_eq!(RequestType::from(42), RequestType::Unsupported(42));
    }

    #[test]
    fn test_status() {
        {
            let status = Status::from_result(Ok(10));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_OK as u8);
            assert_eq!(status.num_used_bytes(), 11);
        }

        {
            let status = Status::from_result(Err(ErrStatus::IoErr(IoErrStatus::BadRequest(
                Error::InvalidOffset,
            ))));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_IOERR as u8);
            assert_eq!(status.num_used_bytes(), 1);
        }

        {
            let status = Status::from_result(Err(ErrStatus::IoErr(IoErrStatus::Flush(
                io::Error::from_raw_os_error(42),
            ))));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_IOERR as u8);
            assert_eq!(status.num_used_bytes(), 1);
        }

        {
            let status = Status::from_result(Err(ErrStatus::IoErr(IoErrStatus::Read(
                0,
                GuestMemoryError::InvalidBackendAddress,
            ))));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_IOERR as u8);
            assert_eq!(status.num_used_bytes(), 1);
        }

        {
            let status = Status::from_result(Err(ErrStatus::IoErr(IoErrStatus::Read(
                10,
                GuestMemoryError::PartialBuffer {
                    expected: 10,
                    completed: 20,
                },
            ))));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_IOERR as u8);
            assert_eq!(status.num_used_bytes(), 11);
        }

        {
            let status = Status::from_result(Err(ErrStatus::IoErr(IoErrStatus::Seek(
                io::Error::from_raw_os_error(42),
            ))));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_IOERR as u8);
            assert_eq!(status.num_used_bytes(), 1);
        }

        {
            let status = Status::from_result(Err(ErrStatus::IoErr(IoErrStatus::Write(
                GuestMemoryError::InvalidBackendAddress,
            ))));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_IOERR as u8);
            assert_eq!(status.num_used_bytes(), 1);
        }

        {
            let status = Status::from_result(Err(ErrStatus::Unsupported(0)));
            assert_eq!(status.virtio_blk_status(), VIRTIO_BLK_S_UNSUPP as u8);
            assert_eq!(status.num_used_bytes(), 1);
        }
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_parse() {
        let m = &GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = VirtQueue::new(GuestAddress(0), &m, 16);

        assert!(vq.end().0 < 0x1000);

        vq.avail.ring[0].set(0);
        vq.avail.idx.set(1);

        {
            let mut q = vq.create_queue();
            // Write only request type descriptor.
            vq.dtable[REQUEST_TYPE_DESCRIPTOR].set(0x1000, 0x1000, VIRTQ_DESC_F_WRITE, 1);
            let request_header = RequestHeader::new(VIRTIO_BLK_T_OUT, 114);
            m.write_obj::<RequestHeader>(request_header, GuestAddress(0x1000))
                .unwrap();
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::UnexpectedWriteOnlyDescriptor)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Chain too short: no DATA_DESCRIPTOR.
            vq.dtable[REQUEST_TYPE_DESCRIPTOR].flags.set(0);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::DescriptorChainTooShort)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Chain too short: no status descriptor.
            vq.dtable[REQUEST_TYPE_DESCRIPTOR]
                .flags
                .set(VIRTQ_DESC_F_NEXT);
            vq.dtable[DATA_DESCRIPTOR].set(0x2000, 0x1000, 0, 2);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::DescriptorChainTooShort)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Write only data for OUT.
            vq.dtable[DATA_DESCRIPTOR]
                .flags
                .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
            vq.dtable[STATUS_DESCRIPTOR].set(0x3000, 0, 0, 0);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::UnexpectedWriteOnlyDescriptor)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Read only data for GetDeviceID.
            m.write_obj::<u32>(VIRTIO_BLK_T_GET_ID, GuestAddress(0x1000))
                .unwrap();
            vq.dtable[DATA_DESCRIPTOR].flags.set(VIRTQ_DESC_F_NEXT);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::UnexpectedReadOnlyDescriptor)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Read only data for IN.
            m.write_obj::<u32>(VIRTIO_BLK_T_IN, GuestAddress(0x1000))
                .unwrap();
            vq.dtable[DATA_DESCRIPTOR].flags.set(VIRTQ_DESC_F_NEXT);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::UnexpectedReadOnlyDescriptor)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Status descriptor not writable.
            vq.dtable[DATA_DESCRIPTOR]
                .flags
                .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::UnexpectedReadOnlyDescriptor)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Status descriptor too small.
            vq.dtable[STATUS_DESCRIPTOR].flags.set(VIRTQ_DESC_F_WRITE);
            assert!(matches!(
                Request::parse(&q.pop(m).unwrap(), m),
                Err(Error::DescriptorLengthTooSmall)
            ));
        }

        {
            let mut q = vq.create_queue();
            // Fix status descriptor length.
            vq.dtable[STATUS_DESCRIPTOR].len.set(0x1000);
            // Invalid guest address for the status descriptor.
            // Parsing will still succeed as the operation that
            // will fail happens when executing the request.
            vq.dtable[STATUS_DESCRIPTOR]
                .addr
                .set(m.last_addr().raw_value());
            assert!(Request::parse(&q.pop(m).unwrap(), m).is_ok());
        }

        {
            let mut q = vq.create_queue();
            // Restore status descriptor.
            vq.dtable[STATUS_DESCRIPTOR].set(0x3000, 0x1000, VIRTQ_DESC_F_WRITE, 0);
            // Invalid guest address for the data descriptor.
            // Parsing will still succeed as the operation that
            // will fail happens when executing the request.
            vq.dtable[DATA_DESCRIPTOR]
                .addr
                .set(m.last_addr().raw_value());
            assert!(Request::parse(&q.pop(m).unwrap(), m).is_ok());
        }

        {
            let mut q = vq.create_queue();
            // Restore data descriptor.
            vq.dtable[DATA_DESCRIPTOR].addr.set(0x2000);
            // Should be OK now.
            let r = Request::parse(&q.pop(m).unwrap(), m).unwrap();
            assert_eq!(r.request_type, RequestType::In);
            assert_eq!(r.sector, 114);
            assert_eq!(r.data_addr, GuestAddress(0x2000));
            assert_eq!(r.data_len, 0x1000);
            assert_eq!(r.status_addr, GuestAddress(0x3000));
        }
    }

    /// -------------------------------------
    /// BEGIN PROPERTY BASED TESTING
    use proptest::arbitrary::Arbitrary;
    use proptest::prelude::*;
    use proptest::strategy::{Map, Strategy, TupleUnion};
    use std::convert::TryInto;

    // Implements a "strategy" for producing arbitrary values of RequestType.
    // This can also be generated by a derive macro from `proptest_derive`, but the crate
    // is currently experimental.
    // Since we are dealing with a very complex type we need to turn off the clippy
    // warning.
    #[allow(clippy::type_complexity)]
    impl Arbitrary for RequestType {
        type Parameters = <u32 as Arbitrary>::Parameters;
        // Tuple union will hold the strategies that we use to generate the request type.
        // The first element is the weight of the strategy, the second is a function that
        // returns the strategy value.
        type Strategy = TupleUnion<(
            (u32, std::sync::Arc<fn() -> Self>),
            (u32, std::sync::Arc<fn() -> Self>),
            (u32, std::sync::Arc<fn() -> Self>),
            (u32, std::sync::Arc<fn() -> Self>),
            (
                u32,
                std::sync::Arc<Map<<u32 as Arbitrary>::Strategy, fn(u32) -> Self>>,
            ),
        )>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            // All strategies have the same weight, there is no reson currently to skew
            // the rations to increase the odds of a specific request type.
            TupleUnion::new((
                (1u32, std::sync::Arc::new(|| RequestType::In {})),
                (1u32, std::sync::Arc::new(|| RequestType::Out {})),
                (1u32, std::sync::Arc::new(|| RequestType::Flush {})),
                (1u32, std::sync::Arc::new(|| RequestType::GetDeviceID {})),
                (
                    1u32,
                    std::sync::Arc::new(Strategy::prop_map(any::<u32>(), |id| {
                        // Random unsupported requests for our implementation start at
                        // VIRTIO_BLK_T_GET_ID + 1 = 9.
                        // This can be further refined to include unsupported requests ids < 9.
                        RequestType::Unsupported(id.checked_add(9).unwrap_or(9))
                    })),
                ),
            ))
        }
    }

    // Descriptor indexes.
    const REQUEST_TYPE_DESCRIPTOR: usize = 0;
    const DATA_DESCRIPTOR: usize = 1;
    const STATUS_DESCRIPTOR: usize = 2;

    impl From<RequestType> for u32 {
        fn from(request_type: RequestType) -> u32 {
            match request_type {
                RequestType::In => VIRTIO_BLK_T_IN,
                RequestType::Out => VIRTIO_BLK_T_OUT,
                RequestType::Flush => VIRTIO_BLK_T_FLUSH,
                RequestType::GetDeviceID => VIRTIO_BLK_T_GET_ID,
                RequestType::Unsupported(id) => id,
            }
        }
    }

    // Returns flags based on the request type.
    fn request_type_flags(request_type: RequestType) -> u16 {
        match request_type {
            RequestType::In => VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            RequestType::Out => VIRTQ_DESC_F_NEXT,
            RequestType::Flush => VIRTQ_DESC_F_NEXT,
            RequestType::GetDeviceID => VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            RequestType::Unsupported(_) => VIRTQ_DESC_F_NEXT,
        }
    }

    fn random_request_parse(
    ) -> impl Strategy<Value = (Result<Request, Error>, GuestMemoryMmap, Queue)> {
        // In this strategy we are going to generate random Requests/Errors and map them
        // to an input descriptor chain.
        //
        // We will check that Request::parse() arrives at the same result after
        // parsing the descriptor chain. Input properties are validated and commented below.
        (
            any::<u64>(), // random data buffer sparsity factor
            any::<u32>(), // data_len
            any::<u64>(), // sector
            any::<RequestType>(),
        )
            .prop_map(|(sparsity, data_len, sector, request_type)| {
                (
                    sparsity,
                    data_len,
                    sector,
                    request_type,
                    request_type.into(),
                )
            })
            .prop_map(
                |(sparsity, data_len, sector, request_type, virtio_request_id)| {
                    // Randomize descriptor addresses. Assumed page size as max buffer len.
                    let base_addr = sparsity & 0x0000_FFFF_FFFF_F000; // 48 bit base, page aligned.
                    let max_desc_len = 0x1000;

                    // First addr starts at page base + 1.
                    let req_type_addr = GuestAddress(base_addr).checked_add(0x1000).unwrap();

                    // Use first 4 bits of randomness to shift the gap size between this descriptor
                    // and the next one.
                    let mut next_desc_dist = max_desc_len + (0x1000 << (sparsity & 0xF));
                    let data_addr = req_type_addr.checked_add(next_desc_dist).unwrap();

                    // Use next 4 bits of randomness to shift gap size between this descriptor
                    // and the next one.
                    next_desc_dist = max_desc_len + (0x1000 << ((sparsity & 0xF0) >> 4));
                    let status_addr = data_addr.checked_add(next_desc_dist).unwrap();

                    let mem_end = status_addr.checked_add(max_desc_len).unwrap();
                    let mem: GuestMemoryMmap = GuestMemoryMmap::from_ranges(&[(
                        GuestAddress(base_addr),
                        (mem_end.0 - base_addr).try_into().unwrap(),
                    )])
                    .unwrap();

                    let vq = VirtQueue::new(GuestAddress(base_addr), &mem, 16);

                    // Begin construction of virtqueue based on the arbitrary request.
                    // Set the head descriptor index(0) in the ring at index 0.
                    vq.avail.ring[0].set(REQUEST_TYPE_DESCRIPTOR as u16);
                    vq.avail.idx.set(1);
                    let q = vq.create_queue();

                    // Craft a random request with the randomized parameters.
                    let mut request = Request {
                        request_type,
                        data_len: data_len & 0xFFF,
                        status_addr,
                        sector,
                        data_addr,
                    };

                    let request_header = RequestHeader::new(virtio_request_id, sector);
                    mem.write_obj::<RequestHeader>(request_header, req_type_addr)
                        .unwrap();

                    // Next descriptor pointer will help us build the chain we expect
                    // to be parsed as above request.
                    // Data desc follows request type desc.
                    let mut next_desc = DATA_DESCRIPTOR;

                    // Flush requests have no data desc.
                    if request.request_type == RequestType::Flush {
                        next_desc = STATUS_DESCRIPTOR;
                        // For flush requests, there should be no data desc, so these fields are 0.
                        request.data_addr = GuestAddress(0);
                        request.data_len = 0;
                    }

                    vq.dtable[REQUEST_TYPE_DESCRIPTOR].set(
                        req_type_addr.0,
                        max_desc_len as u32,
                        VIRTQ_DESC_F_NEXT,
                        next_desc as u16,
                    );

                    if next_desc == DATA_DESCRIPTOR {
                        vq.dtable[DATA_DESCRIPTOR].set(
                            request.data_addr.0,
                            request.data_len,
                            request_type_flags(request.request_type),
                            STATUS_DESCRIPTOR as u16,
                        );
                    }

                    vq.dtable[STATUS_DESCRIPTOR].set(
                        request.status_addr.0,
                        1,
                        VIRTQ_DESC_F_WRITE,
                        0,
                    );

                    // Flip a coin - bit 0 indicates if we are generating a valid request or
                    // an error.
                    if data_len & 0x1 == 0 {
                        // This is the initial correct value.
                        let mut data_desc_flags = vq.dtable[DATA_DESCRIPTOR].flags.get();

                        // Flip coin bit 1 - corrupt the status desc len.
                        if data_len & 0x2 > 0 {
                            vq.dtable[STATUS_DESCRIPTOR].len.set(0);
                            return (Err(Error::DescriptorLengthTooSmall), mem, q);
                        }

                        // Flip coin bit 3 - corrupt data desc next flag.
                        // Exception: flush requests do not have data desc.
                        if data_len & 0x4 > 0 && request.request_type != RequestType::Flush {
                            data_desc_flags &= !VIRTQ_DESC_F_NEXT;
                            vq.dtable[DATA_DESCRIPTOR].flags.set(data_desc_flags);
                            return (Err(Error::DescriptorChainTooShort), mem, q);
                        }

                        // Flip another coin bit 4 - req type desc is write only.
                        if data_len & 0x8 > 0 {
                            vq.dtable[REQUEST_TYPE_DESCRIPTOR].flags.set(
                                vq.dtable[REQUEST_TYPE_DESCRIPTOR].flags.get() | VIRTQ_DESC_F_WRITE,
                            );
                            return (Err(Error::UnexpectedWriteOnlyDescriptor), mem, q);
                        }

                        return match request.request_type {
                            // Readonly buffer is writable.
                            RequestType::Out => {
                                data_desc_flags |= VIRTQ_DESC_F_WRITE;
                                vq.dtable[DATA_DESCRIPTOR].flags.set(data_desc_flags);
                                (Err(Error::UnexpectedWriteOnlyDescriptor), mem, q)
                            }
                            // Writeable buffer is readonly.
                            RequestType::In => {
                                data_desc_flags &= !VIRTQ_DESC_F_WRITE;
                                vq.dtable[DATA_DESCRIPTOR].flags.set(data_desc_flags);
                                (Err(Error::UnexpectedReadOnlyDescriptor), mem, q)
                            }
                            // Writeable buffer is readonly.
                            RequestType::GetDeviceID => {
                                data_desc_flags &= !VIRTQ_DESC_F_WRITE;
                                vq.dtable[DATA_DESCRIPTOR].flags.set(data_desc_flags);
                                (Err(Error::UnexpectedReadOnlyDescriptor), mem, q)
                            }
                            // Simulate no status descriptor.
                            _ => {
                                vq.dtable[REQUEST_TYPE_DESCRIPTOR].flags.set(0);
                                (Err(Error::DescriptorChainTooShort), mem, q)
                            }
                        };
                    }

                    (Ok(request), mem, q)
                },
            )
    }

    macro_rules! assert_err {
        ($expression:expr, $($pattern:tt)+) => {
            match $expression {
                $($pattern)+ => (),
                ref e =>  {
                    println!("expected `{}` but got `{:?}`", stringify!($($pattern)+), e);
                    prop_assert!(false)
                }
            }
        }
    }

    #[test]
    fn parse_random_requests() {
        let cfg = ProptestConfig::with_cases(1000);
        proptest!(cfg, |(mut request in random_request_parse())| {
            let result = Request::parse(&request.2.pop(&request.1).unwrap(), &request.1);
            match result {
                Ok(r) => prop_assert!(r == request.0.unwrap()),
                Err(e) => {
                    // Avoiding implementation of PartialEq which requires that even more types like
                    // GuestMemoryError implement it.
                    match request.0.unwrap_err() {
                        Error::DescriptorChainTooShort => assert_err!(e, Error::DescriptorChainTooShort),
                        Error::DescriptorLengthTooSmall => assert_err!(e, Error::DescriptorLengthTooSmall),
                        Error::UnexpectedWriteOnlyDescriptor => assert_err!(e, Error::UnexpectedWriteOnlyDescriptor),
                        Error::UnexpectedReadOnlyDescriptor => assert_err!(e, Error::UnexpectedReadOnlyDescriptor),
                        _ => unreachable!()
                    }
                }
            }
        });
    }
}
