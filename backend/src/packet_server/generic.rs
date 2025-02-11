//
// Copyright 2021 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::{
    collections::HashMap,
    future::Future,
    net::{SocketAddr, UdpSocket},
    sync::Arc,
};

use anyhow::Result;
use calling_common::Duration;
use log::*;

use crate::{
    metrics::TimingOptions,
    packet_server::SocketLocator,
    sfu::{self, SfuStats},
};

/// The shared state for a generic packet server, only UDP is supported.
///
/// This server is implemented with a single socket for all sends and receives. Multiple threads can
/// use the socket, but this only helps if packet processing takes a long time. Otherwise they'll
/// just block in the kernel trying to send.
pub struct PacketServerState {
    socket: UdpSocket,
    num_threads: usize,
}

impl PacketServerState {
    /// Sets up the server state by binding a socket to `local_addr`.
    pub fn new(
        local_addr_udp: SocketAddr,
        _local_addr_tcp: SocketAddr,
        num_threads: usize,
        _tick_interval: Duration,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            socket: UdpSocket::bind(local_addr_udp)?,
            num_threads,
        }))
    }

    /// Launches the configured number of threads for the server using Tokio's blocking thread pool
    /// ([`tokio::task::spawn_blocking`]).
    ///
    /// `handle_packet` should take a single incoming packet's source address and data and produce a
    /// (possibly empty) set of outgoing packets.
    ///
    /// This should only be called once.
    pub fn start_threads(
        self: Arc<Self>,
        handle_packet: impl FnMut(SocketLocator, &mut [u8]) -> Vec<(Vec<u8>, SocketLocator)>
            + Clone
            + Send
            + 'static,
    ) -> impl Future {
        let all_handles = (0..self.num_threads).map(|_| {
            let self_for_thread = self.clone();
            let handle_packet_for_thread = handle_packet.clone();
            tokio::task::spawn_blocking(move || self_for_thread.run(handle_packet_for_thread))
        });
        futures::future::select_all(all_handles)
    }

    /// Runs a single listener on the current thread.
    ///
    /// See [`PacketServerState::start_threads`].
    fn run(
        self: Arc<Self>,
        mut handle_packet: impl FnMut(SocketLocator, &mut [u8]) -> Vec<(Vec<u8>, SocketLocator)>,
    ) {
        let mut buf = [0u8; 1500];

        loop {
            let received_packet = match self.socket.recv_from(&mut buf) {
                Err(err) => {
                    warn!("recv_from() failed: {}", err);
                    None
                }
                Ok((size, sender_addr)) => Some((size, sender_addr)),
            };

            if let Some((size, sender_addr)) = received_packet {
                let packets_to_send =
                    handle_packet(SocketLocator::Udp(sender_addr), &mut buf[..size]);
                for (buf, addr) in packets_to_send {
                    time_scope!(
                        "calling.udp.generic.send_packet",
                        TimingOptions::nanosecond_1000_per_minute()
                    );
                    sampling_histogram!("calling.generic.send_packet.size_bytes", || buf.len());
                    self.send_packet(&buf, addr);
                }
            }
        }
    }

    pub fn send_packet(&self, buf: &[u8], addr: SocketLocator) {
        match addr {
            SocketLocator::Udp(addr) => {
                trace!("sending packet of {} bytes to {}", buf.len(), addr);
                if let Err(err) = self.socket.send_to(buf, addr) {
                    warn!("send_to failed: {}", err);
                }
            }
            _ => warn!("unable to send packet to {}", addr),
        }
    }

    /// Process the results of [`sfu::Sfu::tick`].
    pub fn tick(&self, tick_update: sfu::TickOutput) -> Result<()> {
        for (buf, addr) in tick_update.packets_to_send {
            self.send_packet(&buf, addr);
        }
        Ok(())
    }

    pub fn get_stats(&self) -> SfuStats {
        let histograms = HashMap::new();
        let values = HashMap::new();
        SfuStats { histograms, values }
    }
}
