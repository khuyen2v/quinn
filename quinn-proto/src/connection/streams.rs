use std::{
    collections::{hash_map, HashMap, VecDeque},
    mem,
};

use bytes::{BufMut, Bytes};
use err_derive::Error;
use tracing::{debug, trace};

use super::{
    assembler::{Assembler, IllegalOrderedRead},
    send_buffer::SendBuffer,
    spaces::Retransmits,
};
use crate::{
    coding::BufMutExt,
    frame::{self, FrameStruct},
    transport_parameters::TransportParameters,
    Dir, Side, StreamId, TransportError, VarInt, MAX_STREAM_COUNT,
};

pub struct Streams {
    side: Side,
    // Set of streams that are currently open, or could be immediately opened by the peer
    send: HashMap<StreamId, Send>,
    recv: HashMap<StreamId, Recv>,
    next: [u64; 2],
    // Locally initiated
    max: [u64; 2],
    // Maximum that can be remotely initiated
    max_remote: [u64; 2],
    // Lowest that hasn't actually been opened
    next_remote: [u64; 2],
    /// Whether the remote endpoint has opened any streams the application doesn't know about yet,
    /// per directionality
    opened: [bool; 2],
    // Next to report to the application, once opened
    next_reported_remote: [u64; 2],
    /// Number of outbound streams
    ///
    /// This differs from `self.send.len()` in that it does not include streams that the peer is
    /// permitted to open but which have not yet been opened.
    send_streams: usize,
    /// Streams with outgoing data queued
    pending: Vec<StreamId>,

    events: VecDeque<StreamEvent>,
    /// Streams blocked on connection-level flow control or stream window space
    ///
    /// Streams are only added to this list when a write fails.
    connection_blocked: Vec<StreamId>,
    /// Connection-level flow control budget dictated by the peer
    max_data: u64,
    /// Limit on incoming data
    local_max_data: u64,
    /// Sum of current offsets of all send streams.
    data_sent: u64,
    /// Sum of end offsets of all receive streams. Includes gaps, so it's an upper bound.
    data_recvd: u64,
    /// Total quantity of unacknowledged outgoing data
    unacked_data: u64,
    /// Configured upper bound for `unacked_data`
    send_window: u64,
    /// Configured upper bound for how much unacked data the peer can send us per stream
    stream_receive_window: u64,
}

impl Streams {
    pub fn new(
        side: Side,
        max_remote_uni: VarInt,
        max_remote_bi: VarInt,
        send_window: u64,
        receive_window: VarInt,
        stream_receive_window: VarInt,
    ) -> Self {
        let mut this = Self {
            side,
            send: HashMap::default(),
            recv: HashMap::default(),
            next: [0, 0],
            max: [0, 0],
            max_remote: [max_remote_bi.into(), max_remote_uni.into()],
            next_remote: [0, 0],
            opened: [false, false],
            next_reported_remote: [0, 0],
            send_streams: 0,
            pending: Vec::new(),
            events: VecDeque::new(),
            connection_blocked: Vec::new(),
            max_data: 0,
            local_max_data: receive_window.into(),
            data_sent: 0,
            data_recvd: 0,
            unacked_data: 0,
            send_window,
            stream_receive_window: stream_receive_window.into(),
        };

        for dir in Dir::iter() {
            for i in 0..this.max_remote[dir as usize] {
                this.insert(None, true, StreamId::new(!side, dir, i));
            }
        }

        this
    }

    pub fn open(&mut self, params: &TransportParameters, dir: Dir) -> Option<StreamId> {
        if self.next[dir as usize] >= self.max[dir as usize] {
            return None;
        }

        self.next[dir as usize] += 1;
        let id = StreamId::new(self.side, dir, self.next[dir as usize] - 1);
        self.insert(Some(params), false, id);
        self.send_streams += 1;
        Some(id)
    }

    pub fn set_params(&mut self, params: &TransportParameters) {
        self.max[Dir::Bi as usize] = params.initial_max_streams_bidi.into();
        self.max[Dir::Uni as usize] = params.initial_max_streams_uni.into();
        self.received_max_data(params.initial_max_data);
        for i in 0..self.max_remote[Dir::Bi as usize] {
            let id = StreamId::new(!self.side, Dir::Bi, i as u64);
            self.send.get_mut(&id).unwrap().max_data =
                params.initial_max_stream_data_bidi_local.into();
        }
    }

    pub fn send_streams(&self) -> usize {
        self.send_streams
    }

    pub fn alloc_remote_stream(&mut self, params: &TransportParameters, dir: Dir) {
        self.max_remote[dir as usize] += 1;
        let id = StreamId::new(!self.side, dir, self.max_remote[dir as usize] - 1);
        self.insert(Some(params), true, id);
    }

    pub fn accept(&mut self, dir: Dir) -> Option<StreamId> {
        if self.next_remote[dir as usize] == self.next_reported_remote[dir as usize] {
            return None;
        }
        let x = self.next_reported_remote[dir as usize];
        self.next_reported_remote[dir as usize] = x + 1;
        if dir == Dir::Bi {
            self.send_streams += 1;
        }
        Some(StreamId::new(!self.side, dir, x))
    }

    pub fn zero_rtt_rejected(&mut self) {
        // Revert to initial state for outgoing streams
        for dir in Dir::iter() {
            for i in 0..self.next[dir as usize] {
                self.send.remove(&StreamId::new(self.side, dir, i)).unwrap();
                if let Dir::Bi = dir {
                    self.recv.remove(&StreamId::new(self.side, dir, i)).unwrap();
                }
            }
            self.next[dir as usize] = 0;
        }
        self.pending.clear();
        self.data_sent = 0;
        self.connection_blocked.clear();
    }

    pub fn read(
        &mut self,
        id: StreamId,
        buf: &mut [u8],
    ) -> Result<Option<(usize, bool)>, ReadError> {
        let mut entry = match self.recv.entry(id) {
            hash_map::Entry::Vacant(_) => return Err(ReadError::UnknownStream),
            hash_map::Entry::Occupied(e) => e,
        };
        let rs = entry.get_mut();
        match rs.read(buf) {
            Ok(Some(len)) => {
                self.local_max_data = self.local_max_data.saturating_add(len as u64);
                Ok(Some((len, rs.receiving_unknown_size())))
            }
            Ok(None) => {
                entry.remove_entry();
                Ok(None)
            }
            Err(e @ ReadError::Reset { .. }) => {
                entry.remove_entry();
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    pub fn read_unordered(
        &mut self,
        id: StreamId,
    ) -> Result<Option<(Bytes, u64, bool)>, ReadError> {
        let mut entry = match self.recv.entry(id) {
            hash_map::Entry::Vacant(_) => return Err(ReadError::UnknownStream),
            hash_map::Entry::Occupied(e) => e,
        };
        let rs = entry.get_mut();
        match rs.read_unordered() {
            Ok(Some((buf, offset))) => {
                self.local_max_data = self.local_max_data.saturating_add(buf.len() as u64);
                Ok(Some((buf, offset, rs.receiving_unknown_size())))
            }
            Ok(None) => {
                entry.remove_entry();
                Ok(None)
            }
            Err(e @ ReadError::Reset { .. }) => {
                entry.remove_entry();
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Queue `data` to be written for `stream`
    pub fn write(&mut self, id: StreamId, data: &[u8]) -> Result<usize, WriteError> {
        let limit = (self.max_data - self.data_sent).min(self.send_window - self.unacked_data);
        let stream = self.send.get_mut(&id).ok_or(WriteError::UnknownStream)?;
        if limit == 0 {
            trace!(stream = %id, "write blocked by connection-level flow control or send window");
            if !stream.connection_blocked {
                stream.connection_blocked = true;
                self.connection_blocked.push(id);
            }
            return Err(WriteError::Blocked);
        }

        let was_pending = stream.is_pending();
        let len = (data.len() as u64).min(limit) as usize;
        let len = stream.write(&data[0..len])?;
        self.data_sent += len as u64;
        self.unacked_data += len as u64;
        trace!(stream = %id, "wrote {} bytes", len);
        if !was_pending {
            self.pending.push(id);
        }
        Ok(len)
    }

    /// Process incoming stream frame
    pub fn received(&mut self, frame: frame::Stream) -> Result<(), TransportError> {
        trace!(id = %frame.id, offset = frame.offset, len = frame.data.len(), fin = frame.fin, "got stream");
        let stream = frame.id;
        self.validate_receive_id(stream).map_err(|e| {
            debug!("received illegal STREAM frame");
            e
        })?;

        let rs = match self.recv.get_mut(&stream) {
            Some(rs) => rs,
            None => {
                trace!("dropping frame for closed stream");
                return Ok(());
            }
        };

        if rs.is_finished() {
            trace!("dropping frame for finished stream");
            return Ok(());
        }

        let new_bytes = rs.ingest(
            frame,
            self.data_recvd,
            self.local_max_data,
            self.stream_receive_window,
        )?;
        self.data_recvd += new_bytes;

        if !rs.assembler.is_stopped() {
            self.on_stream_frame(true, stream);
            return Ok(());
        }

        // We don't buffer data on stopped streams, so issue flow control credit immediately
        self.local_max_data = self.local_max_data.saturating_add(new_bytes);

        // Stopped streams become closed instantly on FIN, so check whether we need to clean up
        if rs.is_closed() {
            self.recv.remove(&stream);
        }
        Ok(())
    }

    /// Process incoming RESET_STREAM frame
    pub fn received_reset(&mut self, frame: frame::ResetStream) -> Result<bool, TransportError> {
        let frame::ResetStream {
            id,
            error_code,
            final_offset,
        } = frame;
        self.validate_receive_id(id).map_err(|e| {
            debug!("received illegal RESET_STREAM frame");
            e
        })?;

        let rs = match self.recv.get_mut(&id) {
            Some(stream) => stream,
            None => {
                trace!("received RESET_STREAM on closed stream");
                return Ok(false);
            }
        };
        let end = rs.assembler.end();

        // Validate final_offset
        if let Some(offset) = rs.final_offset() {
            if offset != final_offset {
                return Err(TransportError::FINAL_SIZE_ERROR("inconsistent value"));
            }
        } else if end > final_offset {
            return Err(TransportError::FINAL_SIZE_ERROR(
                "lower than high water mark",
            ));
        }

        // State transition
        if !rs.reset(error_code, final_offset) {
            // Redundant reset
            return Ok(false);
        }
        let bytes_read = rs.assembler.bytes_read();
        self.on_stream_frame(true, id);

        // Update flow control
        Ok(if bytes_read != final_offset {
            // bytes_read is always <= end, so this won't underflow.
            self.data_recvd += final_offset - end;
            self.local_max_data = self
                .local_max_data
                .saturating_add(final_offset - bytes_read);
            true
        } else {
            false
        })
    }

    /// Process incoming `STOP_SENDING` frame
    pub fn received_stop_sending(&mut self, id: StreamId, error_code: VarInt) {
        let stream = match self.send.get_mut(&id) {
            Some(ss) => ss,
            None => return,
        };
        self.events
            .push_back(StreamEvent::Stopped { id, error_code });
        stream.stop(error_code);
        self.on_stream_frame(false, id);
    }

    /// Set the FIN bit in the next stream frame, generating an empty one if necessary
    pub fn finish(&mut self, id: StreamId) -> Result<(), FinishError> {
        let stream = self.send.get_mut(&id).ok_or(FinishError::UnknownStream)?;
        let was_pending = stream.is_pending();
        stream.finish()?;
        if !was_pending {
            self.pending.push(id);
        }
        Ok(())
    }

    /// Abandon pending and future transmits
    ///
    /// Does not cause the actual RESET_STREAM frame to be sent, just updates internal
    /// state.
    pub fn reset(&mut self, id: StreamId) -> Result<(), UnknownStream> {
        let stream = match self.send.get_mut(&id) {
            Some(ss) => ss,
            None => return Err(UnknownStream { _private: () }),
        };

        if matches!(stream.state, SendState::ResetSent | SendState::ResetRecvd) {
            // Redundant reset call
            return Err(UnknownStream { _private: () });
        }

        // Restore the portion of the send window consumed by the data that we aren't about to
        // send. We leave flow control alone because the peer's responsible for issuing additional
        // credit based on the final offset communicated in the RESET_STREAM frame we send.
        self.unacked_data -= stream.pending.unacked();
        stream.reset();

        // Don't reopen an already-closed stream we haven't forgotten yet
        Ok(())
    }

    pub fn reset_acked(&mut self, id: StreamId) {
        match self.send.entry(id) {
            hash_map::Entry::Vacant(_) => {}
            hash_map::Entry::Occupied(e) => {
                if let SendState::ResetSent = e.get().state {
                    self.send_streams -= 1;
                    e.remove_entry();
                }
            }
        }
    }

    /// Cease accepting data on a stream
    ///
    /// Returns `true` iff the peer should be notified.
    pub fn stop(&mut self, id: StreamId) -> Result<bool, UnknownStream> {
        let stream = match self.recv.get_mut(&id) {
            Some(s) => s,
            None => return Err(UnknownStream { _private: () }),
        };
        stream.assembler.stop();
        // Issue flow control credit for unread data
        self.local_max_data = self
            .local_max_data
            .saturating_add(stream.assembler.end() - stream.assembler.bytes_read());
        Ok(!stream.is_finished())
    }

    pub fn stop_reason(&self, id: StreamId) -> Result<Option<VarInt>, UnknownStream> {
        match self.send.get(&id) {
            Some(s) => Ok(s.stop_reason),
            None => Err(UnknownStream { _private: () }),
        }
    }

    pub fn can_send(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn write_control_frames(
        &mut self,
        buf: &mut Vec<u8>,
        pending: &mut Retransmits,
        sent: &mut Retransmits,
        max_size: usize,
    ) {
        // RESET_STREAM
        while buf.len() + frame::ResetStream::SIZE_BOUND < max_size {
            let (id, error_code) = match pending.reset_stream.pop() {
                Some(x) => x,
                None => break,
            };
            let stream = match self.send.get_mut(&id) {
                Some(x) => x,
                None => continue,
            };
            trace!(stream = %id, "RESET_STREAM");
            sent.reset_stream.push((id, error_code));
            frame::ResetStream {
                id,
                error_code,
                final_offset: stream.offset(),
            }
            .encode(buf);
        }

        // STOP_SENDING
        while buf.len() + frame::StopSending::SIZE_BOUND < max_size {
            let frame = match pending.stop_sending.pop() {
                Some(x) => x,
                None => break,
            };
            let stream = match self.recv.get_mut(&frame.id) {
                Some(x) => x,
                None => continue,
            };
            if stream.is_finished() {
                continue;
            }
            trace!(stream = %frame.id, "STOP_SENDING");
            frame.encode(buf);
            sent.stop_sending.push(frame);
        }

        // MAX_DATA
        if pending.max_data && buf.len() + 9 < max_size {
            pending.max_data = false;
            // We assume that if local_max_data is > VarInt::MAX, it must be due to an
            // astronomically high configured initial credit, so we don't need to worry about the
            // sender actually getting blocked somewhere between the initial credit and the current
            // one.
            if let Ok(max) = VarInt::from_u64(self.local_max_data) {
                trace!(value = self.local_max_data, "MAX_DATA");
                sent.max_data = true;
                buf.write(frame::Type::MAX_DATA);
                buf.write(max);
            }
        }

        // MAX_STREAM_DATA
        while buf.len() + 17 < max_size {
            let id = match pending.max_stream_data.iter().next() {
                Some(x) => *x,
                None => break,
            };
            pending.max_stream_data.remove(&id);
            let rs = match self.recv.get_mut(&id) {
                Some(x) => x,
                None => continue,
            };
            if rs.is_finished() {
                continue;
            }
            sent.max_stream_data.insert(id);
            let max = rs.assembler.bytes_read() + self.stream_receive_window;
            trace!(stream = %id, max = max, "MAX_STREAM_DATA");
            buf.write(frame::Type::MAX_STREAM_DATA);
            buf.write(id);
            buf.write_var(max);
        }

        // MAX_STREAMS_UNI
        if pending.max_uni_stream_id && buf.len() + 9 < max_size {
            pending.max_uni_stream_id = false;
            sent.max_uni_stream_id = true;
            trace!(
                value = self.max_remote[Dir::Uni as usize],
                "MAX_STREAMS (unidirectional)"
            );
            buf.write(frame::Type::MAX_STREAMS_UNI);
            buf.write_var(self.max_remote[Dir::Uni as usize]);
        }

        // MAX_STREAMS_BIDI
        if pending.max_bi_stream_id && buf.len() + 9 < max_size {
            pending.max_bi_stream_id = false;
            sent.max_bi_stream_id = true;
            trace!(
                value = self.max_remote[Dir::Bi as usize],
                "MAX_STREAMS (bidirectional)"
            );
            buf.write(frame::Type::MAX_STREAMS_BIDI);
            buf.write_var(self.max_remote[Dir::Bi as usize]);
        }
    }

    pub fn write_stream_frames(
        &mut self,
        buf: &mut Vec<u8>,
        max_buf_size: usize,
    ) -> Vec<frame::StreamMeta> {
        let mut stream_frames = Vec::new();
        while buf.len() + frame::Stream::SIZE_BOUND < max_buf_size {
            let max_data_len = match max_buf_size.checked_sub(buf.len() + frame::Stream::SIZE_BOUND)
            {
                Some(x) => x,
                None => break,
            };
            let id = match self.pending.pop() {
                Some(x) => x,
                None => break,
            };
            let stream = match self.send.get_mut(&id) {
                Some(s) => s,
                // Stream was reset with pending data and the reset was acknowledged
                None => continue,
            };

            // Reset streams aren't removed from the pending list and still exist while the peer
            // hasn't acknowledged the reset, but should not generate STREAM frames, so we need to
            // check for them explicitly.
            if stream.is_reset() {
                continue;
            }
            let offsets = stream.pending.poll_transmit(max_data_len);
            let fin = offsets.end == stream.pending.offset()
                && matches!(stream.state, SendState::DataSent { .. });
            if fin {
                stream.fin_pending = false;
            }
            if stream.is_pending() {
                self.pending.push(id);
            }

            let meta = frame::StreamMeta { id, offsets, fin };
            trace!(id = %meta.id, off = meta.offsets.start, len = meta.offsets.end - meta.offsets.start, fin = meta.fin, "STREAM");
            meta.encode(true, buf);
            buf.put_slice(stream.pending.get(meta.offsets.clone()));
            stream_frames.push(meta);
        }

        stream_frames
    }

    /// Notify the application that new streams were opened or a stream became readable.
    fn on_stream_frame(&mut self, notify_readable: bool, stream: StreamId) {
        if stream.initiator() == self.side {
            // Notifying about the opening of locally-initiated streams would be redundant.
            if notify_readable {
                self.events.push_back(StreamEvent::Readable { id: stream });
            }
            return;
        }
        let next = &mut self.next_remote[stream.dir() as usize];
        if stream.index() >= *next {
            *next = stream.index() + 1;
            self.opened[stream.dir() as usize] = true;
        } else if notify_readable {
            self.events.push_back(StreamEvent::Readable { id: stream });
        }
    }

    pub fn received_ack_of(&mut self, frame: frame::StreamMeta) {
        let mut entry = match self.send.entry(frame.id) {
            hash_map::Entry::Vacant(_) => return,
            hash_map::Entry::Occupied(e) => e,
        };
        let stream = entry.get_mut();
        if stream.is_reset() {
            // We account for outstanding data on reset streams at time of reset
            return;
        }
        let id = frame.id;
        self.unacked_data -= frame.offsets.end - frame.offsets.start;
        stream.ack(frame);
        if stream.state != SendState::DataRecvd {
            return;
        }

        self.send_streams -= 1;
        entry.remove_entry();
        self.events.push_back(StreamEvent::Finished { id });
    }

    pub fn retransmit(&mut self, frame: frame::StreamMeta) {
        let stream = match self.send.get_mut(&frame.id) {
            // Loss of data on a closed stream is a noop
            None => return,
            Some(x) => x,
        };
        if !stream.is_pending() {
            self.pending.push(frame.id);
        }
        stream.fin_pending |= frame.fin;
        stream.pending.retransmit(frame.offsets);
    }

    pub fn retransmit_all_for_0rtt(&mut self) {
        for dir in Dir::iter() {
            for index in 0..self.next[dir as usize] {
                let id = StreamId::new(Side::Client, dir, index);
                let stream = self.send.get_mut(&id).unwrap();
                if stream.pending.is_fully_acked() && !stream.fin_pending {
                    // Stream data can't be acked in 0-RTT, so we must not have sent anything on
                    // this stream
                    continue;
                }
                if !stream.is_pending() {
                    self.pending.push(id);
                }
                stream.pending.retransmit_all_for_0rtt();
            }
        }
    }

    pub fn received_max_streams(&mut self, dir: Dir, count: u64) -> Result<(), TransportError> {
        if count > MAX_STREAM_COUNT {
            return Err(TransportError::FRAME_ENCODING_ERROR(
                "unrepresentable stream limit",
            ));
        }

        let current = &mut self.max[dir as usize];
        if count > *current {
            *current = count;
            self.events.push_back(StreamEvent::Available { dir });
        }

        Ok(())
    }

    /// Handle increase to connection-level flow control limit
    pub fn received_max_data(&mut self, n: VarInt) {
        self.max_data = self.max_data.max(n.into());
    }

    pub fn received_max_stream_data(
        &mut self,
        id: StreamId,
        offset: u64,
    ) -> Result<(), TransportError> {
        if id.initiator() != self.side && id.dir() == Dir::Uni {
            debug!("got MAX_STREAM_DATA on recv-only {}", id);
            return Err(TransportError::STREAM_STATE_ERROR(
                "MAX_STREAM_DATA on recv-only stream",
            ));
        }

        if let Some(ss) = self.send.get_mut(&id) {
            if ss.increase_max_data(offset) {
                self.events.push_back(StreamEvent::Writable { id });
            }
        } else if id.initiator() == self.side && self.is_local_unopened(id) {
            debug!("got MAX_STREAM_DATA on unopened {}", id);
            return Err(TransportError::STREAM_STATE_ERROR(
                "MAX_STREAM_DATA on unopened stream",
            ));
        }

        self.on_stream_frame(false, id);
        Ok(())
    }

    /// Yield stream events
    pub fn poll(&mut self) -> Option<StreamEvent> {
        if let Some(dir) = Dir::iter().find(|&i| mem::replace(&mut self.opened[i as usize], false))
        {
            return Some(StreamEvent::Opened { dir });
        }

        if let Some(id) = self.poll_unblocked() {
            return Some(StreamEvent::Writable { id });
        }

        self.events.pop_front()
    }

    /// Fetch a stream for which a write previously failed due to *connection-level* flow control or
    /// send window limits which no longer apply.
    fn poll_unblocked(&mut self) -> Option<StreamId> {
        if self.flow_blocked() {
            // Everything's still blocked
            return None;
        }

        while let Some(id) = self.connection_blocked.pop() {
            let stream = match self.send.get_mut(&id) {
                None => continue,
                Some(s) => s,
            };
            debug_assert!(stream.connection_blocked);
            stream.connection_blocked = false;
            // If it's no longer sensible to write to a stream (even to detect an error) then don't
            // report it.
            if stream.is_writable() {
                return Some(id);
            }
        }

        None
    }

    /// Check for errors entailed by the peer's use of `id` as a send stream
    fn validate_receive_id(&mut self, id: StreamId) -> Result<(), TransportError> {
        if self.side == id.initiator() {
            match id.dir() {
                Dir::Uni => {
                    return Err(TransportError::STREAM_STATE_ERROR(
                        "illegal operation on send-only stream",
                    ));
                }
                Dir::Bi if id.index() >= self.next[Dir::Bi as usize] => {
                    return Err(TransportError::STREAM_STATE_ERROR(
                        "operation on unopened stream",
                    ));
                }
                Dir::Bi => {}
            };
        } else {
            let limit = self.max_remote[id.dir() as usize];
            if id.index() >= limit {
                return Err(TransportError::STREAM_LIMIT_ERROR(""));
            }
        }
        Ok(())
    }

    /// Whether a locally initiated stream has never been open
    pub fn is_local_unopened(&self, id: StreamId) -> bool {
        id.index() >= self.next[id.dir() as usize]
    }

    fn insert(&mut self, params: Option<&TransportParameters>, remote: bool, id: StreamId) {
        let bi = id.dir() == Dir::Bi;
        if bi || !remote {
            let max_data = params.map_or(0u32.into(), |params| match id.dir() {
                Dir::Uni => params.initial_max_stream_data_uni,
                // Remote/local appear reversed here because the transport parameters are named from
                // the perspective of the peer.
                Dir::Bi if remote => params.initial_max_stream_data_bidi_local,
                Dir::Bi => params.initial_max_stream_data_bidi_remote,
            });
            let stream = Send::new(max_data);
            assert!(self.send.insert(id, stream).is_none());
        }
        if bi || remote {
            assert!(self.recv.insert(id, Recv::new()).is_none());
        }
    }

    /// Whether application stream writes are currently blocked on connection-level flow control or
    /// the send window
    fn flow_blocked(&self) -> bool {
        self.data_sent >= self.max_data || self.unacked_data >= self.send_window
    }
}

#[derive(Debug)]
struct Send {
    max_data: u64,
    state: SendState,
    pending: SendBuffer,
    /// Whether a frame containing a FIN bit must be transmitted, even if we don't have any new data
    fin_pending: bool,
    /// Whether this stream is in the `connection_blocked` list of `Streams`
    connection_blocked: bool,
    /// The reason the peer wants us to stop, if `STOP_SENDING` was received
    stop_reason: Option<VarInt>,
}

impl Send {
    fn new(max_data: VarInt) -> Self {
        Self {
            max_data: max_data.into(),
            state: SendState::Ready,
            pending: SendBuffer::new(),
            fin_pending: false,
            connection_blocked: false,
            stop_reason: None,
        }
    }

    /// Whether the stream has been reset
    fn is_reset(&self) -> bool {
        matches!(self.state, SendState::ResetSent { .. } | SendState::ResetRecvd { .. })
    }

    fn finish(&mut self) -> Result<(), FinishError> {
        if let Some(error_code) = self.stop_reason {
            Err(FinishError::Stopped(error_code))
        } else if self.state == SendState::Ready {
            self.state = SendState::DataSent {
                finish_acked: false,
            };
            self.fin_pending = true;
            Ok(())
        } else {
            Err(FinishError::UnknownStream)
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        if let Some(error_code) = self.stop_reason {
            return Err(WriteError::Stopped(error_code));
        }
        let budget = self.max_data - self.pending.offset();
        if budget == 0 {
            return Err(WriteError::Blocked);
        }
        let len = (data.len() as u64).min(budget) as usize;
        self.pending.write(&data[0..len]);
        Ok(len)
    }

    /// Update stream state due to a reset sent by the local application
    fn reset(&mut self) {
        use SendState::*;
        if let DataSent { .. } | Ready = self.state {
            self.state = ResetSent;
        }
    }

    /// Handle STOP_SENDING
    fn stop(&mut self, error_code: VarInt) {
        self.stop_reason = Some(error_code);
    }

    fn ack(&mut self, frame: frame::StreamMeta) {
        self.pending.ack(frame.offsets);
        if let SendState::DataSent {
            ref mut finish_acked,
        } = self.state
        {
            *finish_acked |= frame.fin;
            if *finish_acked && self.pending.is_fully_acked() {
                self.state = SendState::DataRecvd;
            }
        }
    }

    /// Handle increase to stream-level flow control limit
    ///
    /// Returns whether the stream was unblocked
    fn increase_max_data(&mut self, offset: u64) -> bool {
        if offset <= self.max_data || self.state != SendState::Ready {
            return false;
        }
        let was_blocked = self.pending.offset() == self.max_data;
        self.max_data = offset;
        was_blocked
    }

    fn offset(&self) -> u64 {
        self.pending.offset()
    }

    fn is_pending(&self) -> bool {
        self.pending.has_unsent_data() || self.fin_pending
    }

    fn is_writable(&self) -> bool {
        use SendState::*;
        // A stream is writable in Ready state or if it's been stopped and the stop hasn't been
        // reported
        match self.state {
            SendState::Ready => true,
            ResetSent | ResetRecvd => self.stop_reason.is_some(),
            _ => false,
        }
    }
}

/// Errors triggered while writing to a send stream
#[derive(Debug, Error, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum WriteError {
    /// The peer is not able to accept additional data, or the connection is congested.
    ///
    /// If the peer issues additional flow control credit, a [`StreamEvent::Writable`] event will
    /// be generated, indicating that retrying the write might succeed.
    ///
    /// [`StreamEvent::Writable`]: crate::StreamEvent::Writable
    #[error(display = "unable to accept further writes")]
    Blocked,
    /// The peer is no longer accepting data on this stream, and it has been implicitly reset. The
    /// stream cannot be finished or further written to.
    ///
    /// Carries an application-defined error code.
    ///
    /// [`StreamEvent::Finished`]: crate::StreamEvent::Finished
    #[error(display = "stopped by peer: code {}", 0)]
    Stopped(VarInt),
    /// Unknown stream
    ///
    /// Occurs when attempting to access a stream after finishing it or observing that it has been
    /// stopped.
    #[error(display = "unknown stream")]
    UnknownStream,
}

#[derive(Debug, Default)]
struct Recv {
    state: RecvState,
    assembler: Assembler,
}

impl Recv {
    fn new() -> Self {
        Self::default()
    }

    fn ingest(
        &mut self,
        frame: frame::Stream,
        received: u64,
        max_data: u64,
        receive_window: u64,
    ) -> Result<u64, TransportError> {
        let end = frame.offset + frame.data.len() as u64;
        if end >= 2u64.pow(62) {
            return Err(TransportError::FLOW_CONTROL_ERROR(
                "maximum stream offset too large",
            ));
        }

        if let Some(final_offset) = self.final_offset() {
            if end > final_offset || (frame.fin && end != final_offset) {
                debug!(end, final_offset, "final size error");
                return Err(TransportError::FINAL_SIZE_ERROR(""));
            }
        }

        let prev_end = self.assembler.end();
        let new_bytes = end.saturating_sub(prev_end);
        let stream_max_data = self.assembler.bytes_read() + receive_window;
        if end > stream_max_data || received + new_bytes > max_data {
            debug!(stream = %frame.id, received, new_bytes, max_data, end, stream_max_data, "flow control error");
            return Err(TransportError::FLOW_CONTROL_ERROR(""));
        }

        if frame.fin {
            if self.assembler.is_stopped() {
                // Stopped streams don't need to wait for the actual data, they just need to know
                // how much there was.
                self.state = RecvState::Closed;
            } else if let RecvState::Recv { ref mut size } = self.state {
                *size = Some(end);
            }
        }

        self.assembler.insert(frame.offset, frame.data);

        Ok(new_bytes)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, ReadError> {
        if self.assembler.is_stopped() {
            return Err(ReadError::UnknownStream);
        }
        let read = self.assembler.read(buf)?;
        if read > 0 {
            Ok(Some(read))
        } else {
            self.read_blocked().map(|()| None)
        }
    }

    fn read_unordered(&mut self) -> Result<Option<(Bytes, u64)>, ReadError> {
        // Return data we already have buffered, regardless of state
        if let Some((offset, bytes)) = self.assembler.read_unordered() {
            Ok(Some((bytes, offset)))
        } else {
            self.read_blocked().map(|()| None)
        }
    }

    fn read_blocked(&mut self) -> Result<(), ReadError> {
        match self.state {
            RecvState::ResetRecvd { error_code, .. } => {
                self.state = RecvState::Closed;
                Err(ReadError::Reset(error_code))
            }
            RecvState::Closed => Err(ReadError::UnknownStream),
            RecvState::Recv { size } => {
                if size == Some(self.assembler.end()) && self.assembler.is_fully_read() {
                    self.state = RecvState::Closed;
                    Ok(())
                } else {
                    Err(ReadError::Blocked)
                }
            }
        }
    }

    fn receiving_unknown_size(&self) -> bool {
        matches!(self.state, RecvState::Recv { size: None })
    }

    /// No more data expected from peer
    fn is_finished(&self) -> bool {
        !matches!(self.state, RecvState::Recv { .. })
    }

    /// All data read by application
    fn is_closed(&self) -> bool {
        self.state == self::RecvState::Closed
    }

    fn final_offset(&self) -> Option<u64> {
        match self.state {
            RecvState::Recv { size } => size,
            RecvState::ResetRecvd { size, .. } => Some(size),
            _ => None,
        }
    }

    /// Returns `false` iff the reset was redundant
    fn reset(&mut self, error_code: VarInt, final_offset: u64) -> bool {
        if matches!(self.state, RecvState::ResetRecvd { .. } | RecvState::Closed) {
            return false;
        }
        self.state = RecvState::ResetRecvd {
            size: final_offset,
            error_code,
        };
        // Nuke buffers so that future reads fail immediately, which ensures future reads don't
        // issue flow control credit redundant to that already issued. We could instead special-case
        // reset streams during read, but it's unclear if there's any benefit to retaining data for
        // reset streams.
        self.assembler.clear();
        true
    }
}

/// Errors triggered when reading from a recv stream
#[derive(Debug, Error, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ReadError {
    /// No more data is currently available on this stream.
    ///
    /// If more data on this stream is received from the peer, an `Event::StreamReadable` will be
    /// generated for this stream, indicating that retrying the read might succeed.
    #[error(display = "blocked")]
    Blocked,
    /// The peer abandoned transmitting data on this stream.
    ///
    /// Carries an application-defined error code.
    #[error(display = "reset by peer: code {}", 0)]
    Reset(VarInt),
    /// Unknown stream
    ///
    /// Occurs when attempting to access a stream after stopping it, or observing that it has been
    /// finished or reset.
    #[error(display = "unknown stream")]
    UnknownStream,
    /// Attempted an ordered read following an unordered read
    ///
    /// Performing an unordered read allows discontinuities to arise in the receive buffer of a
    /// stream which cannot be recovered, making further ordered reads impossible.
    #[error(display = "ordered read after unordered read")]
    IllegalOrderedRead,
}

impl From<IllegalOrderedRead> for ReadError {
    fn from(_: IllegalOrderedRead) -> Self {
        ReadError::IllegalOrderedRead
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum SendState {
    /// Sending new data
    Ready,
    /// Stream was finished; now sending retransmits only
    DataSent { finish_acked: bool },
    /// Sent RESET
    ResetSent,
    /// All sent data acknowledged
    DataRecvd,
    /// Reset acknowledged
    ResetRecvd,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum RecvState {
    Recv { size: Option<u64> },
    ResetRecvd { size: u64, error_code: VarInt },
    Closed,
}

impl Default for RecvState {
    fn default() -> Self {
        RecvState::Recv { size: None }
    }
}

/// Reasons why attempting to finish a stream might fail
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FinishError {
    /// The peer is no longer accepting data on this stream. No
    /// [`StreamEvent::Finished`] event will be emitted for this stream.
    ///
    /// Carries an application-defined error code.
    ///
    /// [`StreamEvent::Finished`]: crate::StreamEvent::Finished
    #[error(display = "stopped by peer: code {}", 0)]
    Stopped(VarInt),
    /// The stream has not yet been created or was already finished or stopped.
    #[error(display = "unknown stream")]
    UnknownStream,
}

/// Application events about streams
#[derive(Debug)]
pub enum StreamEvent {
    /// One or more new streams has been opened
    Opened {
        /// Directionality for which streams have been opened
        dir: Dir,
    },
    /// A currently open stream has data or errors waiting to be read
    Readable {
        /// Which stream is now readable
        id: StreamId,
    },
    /// A formerly write-blocked stream might be ready for a write or have been stopped
    ///
    /// Only generated for streams that are currently open.
    Writable {
        /// Which stream is now writable
        id: StreamId,
    },
    /// A finished stream has been fully acknowledged or stopped
    Finished {
        /// Which stream has been finished
        id: StreamId,
    },
    /// The peer asked us to stop sending on an outgoing stream
    Stopped {
        /// Which stream has been stopped
        id: StreamId,
        /// Error code supplied by the peer
        error_code: VarInt,
    },
    /// At least one new stream of a certain directionality may be opened
    Available {
        /// Directionality for which streams are newly available
        dir: Dir,
    },
}

/// Unknown stream ID
#[derive(Debug)]
pub struct UnknownStream {
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(side: Side) -> Streams {
        Streams::new(
            side,
            128u32.into(),
            128u32.into(),
            1024 * 1024,
            (1024 * 1024u32).into(),
            (1024 * 1024u32).into(),
        )
    }

    #[test]
    fn reset_flow_control() {
        let mut client = make(Side::Client);
        let id = StreamId::new(Side::Server, Dir::Uni, 0);
        let initial_max = client.local_max_data;
        client
            .received(frame::Stream {
                id,
                offset: 0,
                fin: false,
                data: Bytes::from_static(&[0; 2048]),
            })
            .unwrap();
        assert_eq!(client.data_recvd, 2048);
        assert_eq!(client.local_max_data - initial_max, 0);
        client.read(id, &mut [0; 1024]).unwrap();
        assert_eq!(client.local_max_data - initial_max, 1024);
        client
            .received_reset(frame::ResetStream {
                id,
                error_code: 0u32.into(),
                final_offset: 4096,
            })
            .unwrap();
        assert_eq!(client.data_recvd, 4096);
        assert_eq!(client.local_max_data - initial_max, 4096);
    }

    #[test]
    fn reset_after_empty_frame_flow_control() {
        let mut client = make(Side::Client);
        let id = StreamId::new(Side::Server, Dir::Uni, 0);
        let initial_max = client.local_max_data;
        client
            .received(frame::Stream {
                id,
                offset: 4096,
                fin: false,
                data: Bytes::from_static(&[0; 0]),
            })
            .unwrap();
        assert_eq!(client.data_recvd, 4096);
        assert_eq!(client.local_max_data - initial_max, 0);
        client
            .received_reset(frame::ResetStream {
                id,
                error_code: 0u32.into(),
                final_offset: 4096,
            })
            .unwrap();
        assert_eq!(client.data_recvd, 4096);
        assert_eq!(client.local_max_data - initial_max, 4096);
    }

    #[test]
    fn duplicate_reset_flow_control() {
        let mut client = make(Side::Client);
        let id = StreamId::new(Side::Server, Dir::Uni, 0);
        client
            .received_reset(frame::ResetStream {
                id,
                error_code: 0u32.into(),
                final_offset: 4096,
            })
            .unwrap();
        assert_eq!(client.data_recvd, 4096);
        client
            .received_reset(frame::ResetStream {
                id,
                error_code: 0u32.into(),
                final_offset: 4096,
            })
            .unwrap();
        assert_eq!(client.data_recvd, 4096);
    }

    #[test]
    fn stopped() {
        let mut client = make(Side::Client);
        let id = StreamId::new(Side::Server, Dir::Uni, 0);
        let initial_max = client.local_max_data;
        client
            .received(frame::Stream {
                id,
                offset: 0,
                fin: false,
                data: Bytes::from_static(&[0; 32]),
            })
            .unwrap();
        assert_eq!(client.local_max_data, initial_max);
        client.stop(id).unwrap();
        assert_eq!(client.local_max_data - initial_max, 32);
        client
            .received(frame::Stream {
                id,
                offset: 32,
                fin: true,
                data: Bytes::from_static(&[0; 16]),
            })
            .unwrap();
        assert_eq!(client.local_max_data - initial_max, 48);
        assert!(!client.recv.contains_key(&id));
    }
}
