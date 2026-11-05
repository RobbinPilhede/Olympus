use std::mem::size_of;
use std::net::Ipv4Addr;

use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1024 * 1024;

/// The discrete GPU and total RAM reported to clients.
#[derive(Clone, Debug)]
pub struct Hardware {
    pub gpu: Option<String>,
    pub vram_mb: u64,
    pub ram_mb: u64,
}

/// Probes the largest non-software DXGI adapter and total physical memory.
pub fn detect() -> Hardware {
    let (gpu, vram) = largest_adapter().map_or((None, 0), |(name, vram)| (Some(name), vram));
    Hardware {
        gpu,
        vram_mb: vram / MIB,
        ram_mb: memory().map_or(0, |m| m.ullTotalPhys / MIB),
    }
}

/// Bytes of physical memory currently available.
pub fn free_ram() -> u64 {
    memory().map_or(0, |m| m.ullAvailPhys)
}

fn memory() -> Option<MEMORYSTATUSEX> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some(status)
}

fn largest_adapter() -> Option<(String, u64)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut best: Option<(String, u64)> = None;
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let vram = desc.DedicatedVideoMemory as u64;
        if best.as_ref().is_none_or(|(_, b)| vram > *b) {
            let len = desc.Description.iter().position(|c| *c == 0).unwrap_or(desc.Description.len());
            best = Some((String::from_utf16_lossy(&desc.Description[..len]), vram));
        }
    }
    best
}

/// IPv4 addresses of adapters that are up, without loopback or link-local.
pub fn lan_ips() -> Vec<Ipv4Addr> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut len = 16 * 1024u32;
    // u64 elements keep the buffer aligned for IP_ADAPTER_ADDRESSES_LH.
    let mut buf = vec![0u64; len as usize / 8 + 1];
    for _ in 0..3 {
        let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let rc = unsafe { GetAdaptersAddresses(AF_INET.0.into(), flags, None, Some(head), &mut len) };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            buf = vec![0u64; len as usize / 8 + 1];
            continue;
        }
        if rc != 0 {
            return Vec::new();
        }
        return unsafe { walk_adapters(head) };
    }
    Vec::new()
}

unsafe fn walk_adapters(mut adapter: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Ipv4Addr> {
    let mut ips = Vec::new();
    while let Some(a) = unsafe { adapter.as_ref() } {
        if a.OperStatus == IfOperStatusUp && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
            let mut unicast = a.FirstUnicastAddress;
            while let Some(u) = unsafe { unicast.as_ref() } {
                let addr = unsafe { &*u.Address.lpSockaddr.cast::<SOCKADDR_IN>() };
                let ip = Ipv4Addr::from(u32::from_be(unsafe { addr.sin_addr.S_un.S_addr }));
                if !ip.is_loopback() && !ip.is_link_local() {
                    ips.push(ip);
                }
                unicast = u.Next;
            }
        }
        adapter = a.Next;
    }
    ips
}
