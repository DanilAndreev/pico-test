#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use cyw43::{Control, NetDriver};
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{
    Config, IpAddress, IpEndpoint, Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4,
};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, USB};
use embassy_rp::pio::{InterruptHandler as PioIH, Pio};
use embassy_rp::usb::{Driver, InterruptHandler as UsbIH};
use embassy_time::Timer;
use embedded_io_async::Write;
use heapless::String as HString;
use panic_halt as _;
use rand_core::RngCore;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioIH<PIO0>;
    USBCTRL_IRQ => UsbIH<USB>;
});

// ------------------------------------------------------------------
// Configuration
// ------------------------------------------------------------------
const SSID: &str = "PicoBlink";
const WIFI_CHANNEL: u8 = 5;
const SERVER_IP: [u8; 4] = [192, 168, 4, 1];
const CLIENT_IP: [u8; 4] = [192, 168, 4, 42]; // single-client lease

// LED half-period in milliseconds. 500 = 1 Hz blink.
// Atomically updated by the HTTP handler, read by the blink task.
static HALF_PERIOD_MS: AtomicU32 = AtomicU32::new(500);

const INDEX_HTML: &str = include_str!("../assets/index.html");

// ------------------------------------------------------------------
// Background tasks
// ------------------------------------------------------------------
#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn blink_task(mut control: Control<'static>) -> ! {
    loop {
        let half = HALF_PERIOD_MS.load(Ordering::Relaxed) as u64;
        control.gpio_set(0, true).await;
        Timer::after_millis(half).await;
        control.gpio_set(0, false).await;
        Timer::after_millis(half).await;
    }
}

// ------------------------------------------------------------------
// Minimal DHCP server
//
// Handles DISCOVER -> OFFER and REQUEST -> ACK for a single client.
// Hands out CLIENT_IP, advertises us as router/DNS, /24 subnet.
// ------------------------------------------------------------------
#[embassy_executor::task]
async fn dhcp_task(stack: &'static Stack<NetDriver<'static>>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];

    let mut sock =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);

    if sock.bind(67).is_err() {
        log::error!("dhcp: bind(67) failed");
        loop {
            Timer::after_secs(60).await;
        }
    }
    log::info!("dhcp: listening on UDP 67");

    let mut packet = [0u8; 600];
    let mut reply = [0u8; 384];

    loop {
        let (n, _from) = match sock.recv_from(&mut packet).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if n < 240 || packet[0] != 1 {
            continue;
        }

        let xid = [packet[4], packet[5], packet[6], packet[7]];
        let chaddr = {
            let mut a = [0u8; 16];
            a.copy_from_slice(&packet[28..44]);
            a
        };

        let msg_type = match find_dhcp_option(&packet[..n], 53) {
            Some(o) if !o.is_empty() => o[0],
            _ => continue,
        };
        let resp_type = match msg_type {
            1 => 2u8, // DISCOVER -> OFFER
            3 => 5u8, // REQUEST  -> ACK
            _ => continue,
        };

        for b in reply.iter_mut() {
            *b = 0;
        }
        reply[0] = 2; // BOOTREPLY
        reply[1] = 1; // ethernet
        reply[2] = 6; // hlen
        reply[4..8].copy_from_slice(&xid);
        reply[16..20].copy_from_slice(&CLIENT_IP); // yiaddr
        reply[20..24].copy_from_slice(&SERVER_IP); // siaddr
        reply[28..44].copy_from_slice(&chaddr); // chaddr
        reply[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie

        let mut o = 240;
        // 53: DHCP message type
        reply[o] = 53;
        reply[o + 1] = 1;
        reply[o + 2] = resp_type;
        o += 3;
        // 54: server identifier
        reply[o] = 54;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&SERVER_IP);
        o += 6;
        // 51: lease time (1 h)
        reply[o] = 51;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&3600u32.to_be_bytes());
        o += 6;
        // 1: subnet mask
        reply[o] = 1;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&[255, 255, 255, 0]);
        o += 6;
        // 3: router
        reply[o] = 3;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&SERVER_IP);
        o += 6;
        // 6: DNS server (point at ourselves)
        reply[o] = 6;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&SERVER_IP);
        o += 6;
        // 114: captive portal URI (RFC 7710/8910). Modern iOS/Android
        // honor this and skip their HTTP probe — the "Sign in to network"
        // sheet pops up as soon as DHCP completes.
        const PORTAL_URL: &[u8] = b"http://192.168.4.1/";
        reply[o] = 114;
        reply[o + 1] = PORTAL_URL.len() as u8;
        reply[o + 2..o + 2 + PORTAL_URL.len()].copy_from_slice(PORTAL_URL);
        o += 2 + PORTAL_URL.len();
        // 255: end
        reply[o] = 255;
        o += 1;

        let dst = IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), 68);
        let _ = sock.send_to(&reply[..o], dst).await;
        log::info!(
            "dhcp: sent {} for {}.{}.{}.{}",
            if resp_type == 2 { "OFFER" } else { "ACK" },
            CLIENT_IP[0],
            CLIENT_IP[1],
            CLIENT_IP[2],
            CLIENT_IP[3],
        );
    }
}

fn find_dhcp_option(buf: &[u8], code: u8) -> Option<&[u8]> {
    if buf.len() <= 240 {
        return None;
    }
    let mut i = 240;
    while i < buf.len() {
        let opt = buf[i];
        if opt == 255 {
            return None;
        }
        if opt == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= buf.len() {
            return None;
        }
        let len = buf[i + 1] as usize;
        if i + 2 + len > buf.len() {
            return None;
        }
        if opt == code {
            return Some(&buf[i + 2..i + 2 + len]);
        }
        i += 2 + len;
    }
    None
}

// ------------------------------------------------------------------
// Minimal DNS server (captive portal hijack)
//
// Returns SERVER_IP as the A record for *any* hostname. AAAA and other
// query types get an empty (NOERROR/no-data) reply. This is what makes
// the OS captive-portal probes hit our HTTP server.
// ------------------------------------------------------------------
#[embassy_executor::task]
async fn dns_task(stack: &'static Stack<NetDriver<'static>>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];

    let mut sock =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);

    if sock.bind(53).is_err() {
        log::error!("dns: bind(53) failed");
        loop {
            Timer::after_secs(60).await;
        }
    }
    log::info!("dns: listening on UDP 53");

    let mut packet = [0u8; 512];
    let mut reply = [0u8; 512];

    loop {
        let (n, from) = match sock.recv_from(&mut packet).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if n < 12 {
            continue;
        }

        // Walk question name to find QTYPE/QCLASS and end of question.
        let mut p = 12;
        let mut name_ok = false;
        while p < n {
            let len = packet[p] as usize;
            if len == 0 {
                p += 1;
                name_ok = true;
                break;
            }
            if len & 0xc0 == 0xc0 {
                p += 2;
                name_ok = true;
                break;
            }
            p += 1 + len;
        }
        if !name_ok || p + 4 > n {
            continue;
        }
        let qtype = u16::from_be_bytes([packet[p], packet[p + 1]]);
        p += 4; // qtype + qclass
        let q_end = p;

        // Build reply.
        for b in reply.iter_mut() {
            *b = 0;
        }
        reply[0] = packet[0]; // ID
        reply[1] = packet[1];
        reply[2] = 0x81; // QR=1, Opcode=0, AA=0, TC=0, RD=copied
        reply[3] = 0x80; // RA=1, Z=0, RCODE=0
        reply[4] = 0; // QDCOUNT = 1
        reply[5] = 1;
        let answer_a = qtype == 1; // only synthesize for A
        reply[6] = 0; // ANCOUNT
        reply[7] = if answer_a { 1 } else { 0 };
        // NSCOUNT/ARCOUNT = 0

        // Echo the question section back.
        let q_start = 12;
        let q_len = q_end - q_start;
        reply[q_start..q_start + q_len].copy_from_slice(&packet[q_start..q_end]);
        let mut out = q_start + q_len;

        if answer_a {
            // Name: pointer to offset 12 (start of question name).
            reply[out] = 0xc0;
            reply[out + 1] = 0x0c;
            // TYPE = A
            reply[out + 2] = 0;
            reply[out + 3] = 1;
            // CLASS = IN
            reply[out + 4] = 0;
            reply[out + 5] = 1;
            // TTL = 60s
            reply[out + 6..out + 10].copy_from_slice(&60u32.to_be_bytes());
            // RDLENGTH = 4
            reply[out + 10] = 0;
            reply[out + 11] = 4;
            // RDATA = 192.168.4.1
            reply[out + 12..out + 16].copy_from_slice(&SERVER_IP);
            out += 16;
        }

        let _ = sock.send_to(&reply[..out], from).await;
    }
}

// ------------------------------------------------------------------
// HTTP server
// ------------------------------------------------------------------
// Pool size 4 lets us accept 4 parallel TCP connections on :80.
// iOS's captive sheet opens several at once (page + reachability probes);
// a single-socket server hits "could not connect" on the parallel ones.
#[embassy_executor::task(pool_size = 4)]
async fn http_task(stack: &'static Stack<NetDriver<'static>>) -> ! {
    let mut rx = [0u8; 1536];
    let mut tx = [0u8; 1536];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(15)));

        if socket.accept(80).await.is_err() {
            continue;
        }
        log::info!("http: accepted");

        let _ = serve(&mut socket).await;
        // Drain pending bytes BEFORE sending FIN so the response is fully
        // transmitted (close() queues the FIN; flush() waits for ACK).
        let _ = socket.flush().await;
        socket.close();
    }
}

async fn serve(s: &mut TcpSocket<'_>) -> Result<(), ()> {
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).await.map_err(|_| ())?;
    let req = core::str::from_utf8(&buf[..n]).unwrap_or("");
    let line = req.lines().next().unwrap_or("");
    log::info!("http: {}", line);

    if line.starts_with("GET /set") {
        if let Some(hz) = parse_hz(line) {
            let hz = hz.clamp(1, 20);
            HALF_PERIOD_MS.store(500 / hz, Ordering::Relaxed);
            log::info!("http: blink rate -> {hz} Hz");
        }
        redirect(s, "/").await
    } else if line.starts_with("GET /") {
        index(s).await
    } else {
        status(s, 404, "Not Found").await
    }
}

fn parse_hz(line: &str) -> Option<u32> {
    let q = line.find('?')?;
    let end_rel = line[q..].find(' ').unwrap_or(line.len() - q);
    let query = &line[q + 1..q + end_rel];
    for part in query.split('&') {
        if let Some(v) = part.strip_prefix("hz=") {
            return v.parse().ok();
        }
    }
    None
}

async fn index(s: &mut TcpSocket<'_>) -> Result<(), ()> {
    let half = HALF_PERIOD_MS.load(Ordering::Relaxed).max(1);
    let hz = 500 / half;

    let mut hz_str: HString<8> = HString::new();
    let _ = write!(hz_str, "{hz}");

    // Template substitution: HTML has two {{HZ}} markers.
    let parts: heapless::Vec<&str, 3> = INDEX_HTML.split("{{HZ}}").collect();
    let body_len: usize =
        parts.iter().map(|p| p.len()).sum::<usize>() + hz_str.len() * (parts.len() - 1);

    let mut header: HString<160> = HString::new();
    let _ = write!(
        header,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n",
    );
    s.write_all(header.as_bytes()).await.map_err(|_| ())?;

    for (i, part) in parts.iter().enumerate() {
        s.write_all(part.as_bytes()).await.map_err(|_| ())?;
        if i + 1 < parts.len() {
            s.write_all(hz_str.as_bytes()).await.map_err(|_| ())?;
        }
    }
    Ok(())
}

async fn redirect(s: &mut TcpSocket<'_>, loc: &str) -> Result<(), ()> {
    let mut header: HString<128> = HString::new();
    let _ = write!(
        header,
        "HTTP/1.1 303 See Other\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    s.write_all(header.as_bytes()).await.map_err(|_| ())?;
    Ok(())
}

async fn status(s: &mut TcpSocket<'_>, code: u16, reason: &str) -> Result<(), ()> {
    let mut header: HString<128> = HString::new();
    let _ = write!(
        header,
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    s.write_all(header.as_bytes()).await.map_err(|_| ())?;
    Ok(())
}

// ------------------------------------------------------------------
// Entry point
// ------------------------------------------------------------------
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // USB-CDC serial logger (so we can watch DHCP/HTTP events).
    spawner.spawn(logger_task(Driver::new(p.USB, Irqs))).unwrap();
    Timer::after_millis(200).await;
    log::info!("Pico W booting");

    // CYW43 firmware blobs.
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    // PIO-SPI link to the wireless chip.
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(cyw43_task(runner)).unwrap();
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    log::info!("Starting open AP: SSID='{SSID}', channel {WIFI_CHANNEL}");
    control.start_ap_open(SSID, WIFI_CHANNEL).await;

    // Embassy-net stack with static IP. We act as gateway/DNS/server.
    let config = Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(
            Ipv4Address::new(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3]),
            24,
        ),
        gateway: Some(Ipv4Address::new(
            SERVER_IP[0],
            SERVER_IP[1],
            SERVER_IP[2],
            SERVER_IP[3],
        )),
        dns_servers: heapless::Vec::new(),
    });

    let seed = RoscRng.next_u64();
    // Sockets: 4 HTTP + 2 UDP (DHCP, DNS) + headroom for parallel accepts.
    static RESOURCES: StaticCell<StackResources<12>> = StaticCell::new();
    static STACK: StaticCell<Stack<NetDriver<'static>>> = StaticCell::new();
    let stack = STACK.init(Stack::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<12>::new()),
        seed,
    ));

    spawner.spawn(net_task(stack)).unwrap();
    spawner.spawn(dhcp_task(stack)).unwrap();
    spawner.spawn(dns_task(stack)).unwrap();
    // Four HTTP worker tasks, each holding its own listening TCP socket.
    for _ in 0..4 {
        spawner.spawn(http_task(stack)).unwrap();
    }
    spawner.spawn(blink_task(control)).unwrap();

    log::info!("AP up. Join '{SSID}' WiFi, then open http://192.168.4.1/");
    loop {
        Timer::after_secs(30).await;
        let hz = 500 / HALF_PERIOD_MS.load(Ordering::Relaxed).max(1);
        log::info!("alive; current blink rate {hz} Hz");
    }
}
