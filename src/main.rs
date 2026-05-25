#![no_std]
#![no_main]

mod settings;

use core::fmt::Write as _;

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
use embassy_rp::flash::{Blocking, Flash};
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

use crate::settings::SettingsStore;

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
const CLIENT_IP: [u8; 4] = [192, 168, 4, 42];

const FLASH_SIZE: usize = 2 * 1024 * 1024;

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
async fn blink_task(mut control: Control<'static>, settings: &'static SettingsStore) -> ! {
    loop {
        let half = settings.blink_half_period_ms() as u64;
        control.gpio_set(0, true).await;
        Timer::after_millis(half).await;
        control.gpio_set(0, false).await;
        Timer::after_millis(half).await;
    }
}

// ------------------------------------------------------------------
// DHCP server
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
        reply[16..20].copy_from_slice(&CLIENT_IP);
        reply[20..24].copy_from_slice(&SERVER_IP);
        reply[28..44].copy_from_slice(&chaddr);
        reply[236..240].copy_from_slice(&[99, 130, 83, 99]);

        let mut o = 240;
        reply[o] = 53;
        reply[o + 1] = 1;
        reply[o + 2] = resp_type;
        o += 3;
        reply[o] = 54;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&SERVER_IP);
        o += 6;
        reply[o] = 51;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&3600u32.to_be_bytes());
        o += 6;
        reply[o] = 1;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&[255, 255, 255, 0]);
        o += 6;
        reply[o] = 3;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&SERVER_IP);
        o += 6;
        reply[o] = 6;
        reply[o + 1] = 4;
        reply[o + 2..o + 6].copy_from_slice(&SERVER_IP);
        o += 6;
        // Option 114: captive portal URL (RFC 7710/8910).
        const PORTAL_URL: &[u8] = b"http://192.168.4.1/";
        reply[o] = 114;
        reply[o + 1] = PORTAL_URL.len() as u8;
        reply[o + 2..o + 2 + PORTAL_URL.len()].copy_from_slice(PORTAL_URL);
        o += 2 + PORTAL_URL.len();
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
// DNS server (captive portal hijack)
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
        p += 4;
        let q_end = p;

        for b in reply.iter_mut() {
            *b = 0;
        }
        reply[0] = packet[0];
        reply[1] = packet[1];
        reply[2] = 0x81;
        reply[3] = 0x80;
        reply[4] = 0;
        reply[5] = 1;
        let answer_a = qtype == 1;
        reply[6] = 0;
        reply[7] = if answer_a { 1 } else { 0 };

        let q_start = 12;
        let q_len = q_end - q_start;
        reply[q_start..q_start + q_len].copy_from_slice(&packet[q_start..q_end]);
        let mut out = q_start + q_len;

        if answer_a {
            reply[out] = 0xc0;
            reply[out + 1] = 0x0c;
            reply[out + 2] = 0;
            reply[out + 3] = 1;
            reply[out + 4] = 0;
            reply[out + 5] = 1;
            reply[out + 6..out + 10].copy_from_slice(&60u32.to_be_bytes());
            reply[out + 10] = 0;
            reply[out + 11] = 4;
            reply[out + 12..out + 16].copy_from_slice(&SERVER_IP);
            out += 16;
        }

        let _ = sock.send_to(&reply[..out], from).await;
    }
}

// ------------------------------------------------------------------
// HTTP server
// ------------------------------------------------------------------
#[embassy_executor::task(pool_size = 4)]
async fn http_task(
    stack: &'static Stack<NetDriver<'static>>,
    settings: &'static SettingsStore,
) -> ! {
    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 2048];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(15)));

        if socket.accept(80).await.is_err() {
            continue;
        }
        log::info!("http: accepted");

        let _ = serve(&mut socket, settings).await;
        let _ = socket.flush().await;
        socket.close();
    }
}

async fn serve(s: &mut TcpSocket<'_>, settings: &'static SettingsStore) -> Result<(), ()> {
    let mut buf = [0u8; 2048];
    let n = s.read(&mut buf).await.map_err(|_| ())?;
    let req = core::str::from_utf8(&buf[..n]).unwrap_or("");
    let line = req.lines().next().unwrap_or("");
    log::info!("http: {}", line);

    // --- Form: quick blink frequency change ---
    if line.starts_with("GET /set") {
        if let Some(hz) = parse_hz(line) {
            if let Err(e) = settings.set_blink_hz(hz).await {
                log::warn!("settings: set_blink_hz failed: {:?}", e);
            } else {
                log::info!("settings: blink_hz -> {hz}");
            }
        }
        return redirect(s, "/").await;
    }

    // --- API: reset to embedded defaults ---
    if line.starts_with("POST /api/reset") || line.starts_with("GET /api/reset") {
        match settings.reset().await {
            Ok(_) => log::info!("settings: reset to defaults"),
            Err(e) => log::warn!("settings: reset failed: {:?}", e),
        }
        return redirect(s, "/").await;
    }

    // --- API: import partial JSON ---
    if line.starts_with("POST /api/settings") {
        let body = extract_body(req).unwrap_or("");
        match settings.import_json(body.as_bytes()).await {
            Ok(_) => {
                log::info!("settings: imported {} bytes of JSON", body.len());
                return status(s, 200, "OK").await;
            }
            Err(e) => {
                log::warn!("settings: import failed: {:?}", e);
                return status(s, 400, "Bad Request").await;
            }
        }
    }

    // --- API: export current settings as JSON ---
    if line.starts_with("GET /api/settings") {
        let mut buf = [0u8; 256];
        return match settings.export_json(&mut buf).await {
            Ok(slice) => send_json(s, slice).await,
            Err(_) => status(s, 500, "Internal Error").await,
        };
    }

    // --- Anything else: serve the settings page (captive portal hits land here) ---
    if line.starts_with("GET /") {
        return index(s, settings).await;
    }

    status(s, 404, "Not Found").await
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

/// Returns the body slice from a parsed HTTP request (everything after the
/// first `\r\n\r\n`). For requests larger than the read buffer this only
/// returns what we got — fine for our small JSON payloads.
fn extract_body(req: &str) -> Option<&str> {
    req.split_once("\r\n\r\n").map(|(_, body)| body)
}

async fn index(s: &mut TcpSocket<'_>, settings: &'static SettingsStore) -> Result<(), ()> {
    let snapshot = settings.current().await;

    let mut hz_str: HString<8> = HString::new();
    let _ = write!(hz_str, "{}", snapshot.blink_hz);

    // INDEX_HTML may contain multiple {{HZ}} placeholders.
    let parts: heapless::Vec<&str, 8> = INDEX_HTML.split("{{HZ}}").collect();
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

async fn send_json(s: &mut TcpSocket<'_>, body: &[u8]) -> Result<(), ()> {
    let mut header: HString<160> = HString::new();
    let _ = write!(
        header,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    s.write_all(header.as_bytes()).await.map_err(|_| ())?;
    s.write_all(body).await.map_err(|_| ())?;
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

    spawner.spawn(logger_task(Driver::new(p.USB, Irqs))).unwrap();
    Timer::after_millis(200).await;
    log::info!("Pico W booting");

    // Initialize flash + settings store. Reads the last sector; falls back
    // to compile-time defaults from default-settings.json on first boot.
    let flash = Flash::<_, Blocking, FLASH_SIZE>::new_blocking(p.FLASH);
    static SETTINGS_CELL: StaticCell<SettingsStore> = StaticCell::new();
    let settings: &'static SettingsStore = SETTINGS_CELL.init(SettingsStore::init(flash));

    // CYW43 wireless chip
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");
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

    // Network stack
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
    for _ in 0..4 {
        spawner.spawn(http_task(stack, settings)).unwrap();
    }
    spawner.spawn(blink_task(control, settings)).unwrap();

    log::info!("AP up. Join '{SSID}' WiFi, then open http://192.168.4.1/");
    loop {
        Timer::after_secs(30).await;
        let snap = settings.current().await;
        log::info!("alive; current blink {} Hz", snap.blink_hz);
    }
}
