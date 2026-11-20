//! What goes over the input channel: every connection counts the bytes it
//! sends and receives, and the side that connects measures how long its
//! pings take to be answered.
//!
//! No new protocol is needed: the connect side already pings every peer twice
//! a second to notice a dead connection (`connect::ping_pong`), and every
//! version answers with a pong. A connection is wrapped in a [`MeteredConn`]
//! that recognizes the two and times them, so it works with Lan Mouse too.

use std::{
    any::Any,
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use async_trait::async_trait;
use rustinput_proto::EventType;
use webrtc_util::Conn;

/// Bytes a datagram takes on the network on top of its events, an estimate:
/// a DTLS 1.2 record with AES-GCM (13 bytes of header, 8 of nonce and 16 of
/// tag), the UDP header (8) and the IPv4 header (20).
pub(crate) const DATAGRAM_OVERHEAD: u64 = 13 + 8 + 16 + 8 + 20;

type ArcConn = Arc<dyn Conn + Send + Sync>;

/// The counters of one connection.
#[derive(Default)]
pub(crate) struct Meter {
    sent: AtomicU64,
    received: AtomicU64,
    pings: Mutex<Pings>,
}

/// the pings of a connection, and the answers to them
#[derive(Default)]
struct Pings {
    /// when the ping still waiting for its pong was sent
    pending: Option<Instant>,
    /// round trip times not yet reported, in microseconds
    rtts: Vec<u32>,
    /// pings that got no pong before the next one, not yet reported
    lost: u32,
}

impl Meter {
    fn sent(&self, datagram: &[u8]) {
        self.sent.fetch_add(wire_size(datagram), Ordering::Relaxed);
        if is(datagram, EventType::Ping) {
            let mut pings = self.pings.lock().expect("lock");
            // the previous ping is half a second old: no answer came
            if pings.pending.replace(Instant::now()).is_some() {
                pings.lost += 1;
            }
        }
    }

    fn received(&self, datagram: &[u8]) {
        self.received
            .fetch_add(wire_size(datagram), Ordering::Relaxed);
        if is(datagram, EventType::Pong) {
            let mut pings = self.pings.lock().expect("lock");
            // only pongs answering our pings count: the listen side
            // answers pings and sends none
            if let Some(sent) = pings.pending.take() {
                let rtt = sent.elapsed().as_micros().min(u32::MAX as u128) as u32;
                pings.rtts.push(rtt);
            }
        }
    }

    /// the totals so far, and the pings since the last call
    fn take(&self) -> Counts {
        let mut pings = self.pings.lock().expect("lock");
        Counts {
            sent: self.sent.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
            rtts: std::mem::take(&mut pings.rtts),
            lost: std::mem::take(&mut pings.lost),
        }
    }
}

/// size of `datagram` on the network
fn wire_size(datagram: &[u8]) -> u64 {
    datagram.len() as u64 + DATAGRAM_OVERHEAD
}

/// whether `datagram` is an event of type `kind`
fn is(datagram: &[u8], kind: EventType) -> bool {
    datagram.first() == Some(&(kind as u8))
}

/// A connection counting what goes through it into its [`Meter`].
pub(crate) struct MeteredConn {
    inner: ArcConn,
    meter: Arc<Meter>,
}

#[async_trait]
impl Conn for MeteredConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        self.inner.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        let len = self.inner.recv(buf).await?;
        self.meter.received(&buf[..len]);
        Ok(len)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        let (len, addr) = self.inner.recv_from(buf).await?;
        self.meter.received(&buf[..len]);
        Ok((len, addr))
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let len = self.inner.send(buf).await?;
        self.meter.sent(buf);
        Ok(len)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        let len = self.inner.send_to(buf, target).await?;
        self.meter.sent(buf);
        Ok(len)
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner.remote_addr()
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        self.inner.close().await
    }

    /// the wrapped connection: `DTLSConn` is downcast to read the
    /// certificate of the peer
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self.inner.as_any()
    }
}

/// what a meter counted, see [`Meter::take`]
struct Counts {
    sent: u64,
    received: u64,
    rtts: Vec<u32>,
    lost: u32,
}

/// One second (or so) of the connections with one device.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Sample {
    pub(crate) ip: Option<IpAddr>,
    /// bytes since the previous sample
    pub(crate) sent: u64,
    pub(crate) received: u64,
    /// bytes since the connections were made
    pub(crate) sent_total: u64,
    pub(crate) received_total: u64,
    /// round trip times of the pings answered since the previous sample
    pub(crate) rtts: Vec<u32>,
    pub(crate) lost: u32,
}

impl Sample {
    /// the average round trip time, in microseconds
    pub(crate) fn rtt(&self) -> Option<u32> {
        let n = self.rtts.len() as u64;
        (n > 0).then(|| (self.rtts.iter().map(|&r| r as u64).sum::<u64>() / n) as u32)
    }
}

/// a connection being measured
struct Link {
    addr: SocketAddr,
    meter: Weak<Meter>,
    /// the totals at the previous sample
    sent: u64,
    received: u64,
}

/// The meters of all connections of the input channel, both the ones this
/// device made and the ones it accepted.
#[derive(Clone, Default)]
pub(crate) struct NetStats {
    links: Arc<Mutex<Vec<Link>>>,
}

impl NetStats {
    /// `conn` to `addr`, counted from now on
    pub(crate) fn meter(&self, addr: SocketAddr, conn: ArcConn) -> ArcConn {
        let meter = Arc::new(Meter::default());
        self.links.lock().expect("lock").push(Link {
            addr,
            meter: Arc::downgrade(&meter),
            sent: 0,
            received: 0,
        });
        Arc::new(MeteredConn { inner: conn, meter })
    }

    /// What went over the connections since the previous call, one sample
    /// per device: a device that is a client here and has this one as a
    /// client too has a connection each way. Closed connections are
    /// forgotten.
    pub(crate) fn sample(&self) -> Vec<Sample> {
        let mut links = self.links.lock().expect("lock");
        let mut devices: BTreeMap<IpAddr, Sample> = BTreeMap::new();
        links.retain_mut(|link| {
            let Some(meter) = link.meter.upgrade() else {
                return false;
            };
            let counts = meter.take();
            let sample = devices.entry(link.addr.ip()).or_insert_with(|| Sample {
                ip: Some(link.addr.ip()),
                ..Default::default()
            });
            sample.sent += counts.sent - link.sent;
            sample.received += counts.received - link.received;
            sample.sent_total += counts.sent;
            sample.received_total += counts.received;
            sample.rtts.extend(counts.rtts);
            sample.lost += counts.lost;
            link.sent = counts.sent;
            link.received = counts.received;
            true
        });
        devices.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([192, 168, 1, last], port))
    }

    const PING: [u8; 1] = [EventType::Ping as u8];
    const PONG: [u8; 2] = [EventType::Pong as u8, 1];

    #[test]
    fn pings_are_timed_by_their_pongs() {
        let meter = Meter::default();
        meter.sent(&PING);
        std::thread::sleep(std::time::Duration::from_millis(5));
        meter.received(&PONG);
        // a second pong for the same ping is not timed
        meter.received(&PONG);
        let counts = meter.take();
        assert_eq!(counts.rtts.len(), 1);
        assert!(counts.rtts[0] >= 5_000, "{:?}", counts.rtts);
        assert_eq!(counts.lost, 0);
        assert_eq!(counts.sent, 1 + DATAGRAM_OVERHEAD);
        assert_eq!(counts.received, 2 * (2 + DATAGRAM_OVERHEAD));
        // taken: reported once
        assert!(meter.take().rtts.is_empty());
    }

    #[test]
    fn a_ping_without_pong_is_lost() {
        let meter = Meter::default();
        meter.sent(&PING);
        meter.sent(&PING);
        meter.sent(&PING);
        meter.received(&PONG);
        let counts = meter.take();
        assert_eq!(counts.lost, 2);
        assert_eq!(counts.rtts.len(), 1);
    }

    #[test]
    fn the_listen_side_times_nothing() {
        // it receives pings and answers them
        let meter = Meter::default();
        meter.received(&PING);
        meter.sent(&PONG);
        let counts = meter.take();
        assert!(counts.rtts.is_empty());
        assert_eq!(counts.lost, 0);
    }

    #[test]
    fn samples_add_up_the_connections_of_a_device() {
        let stats = NetStats::default();
        // both ways with .50, one connection with .51
        let out = Arc::new(Meter::default());
        let back = Arc::new(Meter::default());
        let other = Arc::new(Meter::default());
        {
            let mut links = stats.links.lock().unwrap();
            for (a, m) in [
                (addr(50, 4242), &out),
                (addr(50, 50123), &back),
                (addr(51, 4242), &other),
            ] {
                links.push(Link {
                    addr: a,
                    meter: Arc::downgrade(m),
                    sent: 0,
                    received: 0,
                });
            }
        }
        out.sent(&[0; 21]);
        back.received(&[0; 21]);
        other.sent(&PING);
        other.received(&PONG);

        let samples = stats.sample();
        assert_eq!(samples.len(), 2);
        let first = &samples[0];
        assert_eq!(first.ip, Some(addr(50, 0).ip()));
        assert_eq!(first.sent, 21 + DATAGRAM_OVERHEAD);
        assert_eq!(first.received, 21 + DATAGRAM_OVERHEAD);
        assert_eq!(samples[1].rtts.len(), 1);
        assert!(samples[1].rtt().is_some());

        // the next sample has only what came after, the totals keep growing
        out.sent(&[0; 9]);
        let samples = stats.sample();
        assert_eq!(samples[0].sent, 9 + DATAGRAM_OVERHEAD);
        assert_eq!(samples[0].sent_total, 30 + 2 * DATAGRAM_OVERHEAD);
        assert_eq!(samples[1].sent, 0);

        // a closed connection is forgotten
        drop(other);
        assert_eq!(stats.sample().len(), 1);
    }
}
