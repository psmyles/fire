//! Phase-0 GPU probe. Enumerates wgpu adapters, prints each adapter's
//! `max_texture_dimension_2d` (which drives the §6 downscale-to-fit limit — read it,
//! never hardcode 16384), and confirms the fallback adapter is available (the §5.1
//! device-loss recovery path requests `force_fallback_adapter = true`).
//!
//! Run: `cargo run -p fire-daemon --example adapter_probe`
//!
//! wgpu-29 API notes (verified against the crate source): `Instance::new` takes the
//! descriptor by value and `InstanceDescriptor` has no `Default`, so we use
//! `Instance::default()` (all backends). `enumerate_adapters` is async (returns a
//! `Future<Output = Vec<Adapter>>`), so it is blocked on with pollster.
//! `request_adapter` returns `Result<Adapter, RequestAdapterError>`.

fn main() {
    let instance = wgpu::Instance::default();

    println!("=== wgpu adapters ===");
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    if adapters.is_empty() {
        println!("  (none found)");
    }
    for a in &adapters {
        let info = a.get_info();
        let limits = a.limits();
        println!(
            "  [{:?}] {} via {:?} — max_texture_dimension_2d = {}",
            info.device_type, info.name, info.backend, limits.max_texture_dimension_2d
        );
    }

    println!("\n=== default high-performance adapter ===");
    match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        ..Default::default()
    })) {
        Ok(a) => {
            let info = a.get_info();
            println!(
                "  OK: {} via {:?}, max_texture_dimension_2d = {}",
                info.name,
                info.backend,
                a.limits().max_texture_dimension_2d
            );
        }
        Err(e) => println!("  FAILED: {e:?}"),
    }

    println!("\n=== fallback adapter (device-loss recovery path, §5.1) ===");
    match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: true,
        ..Default::default()
    })) {
        Ok(a) => {
            let info = a.get_info();
            println!("  OK: {} via {:?}", info.name, info.backend);
        }
        Err(e) => println!("  NOT AVAILABLE: {e:?}"),
    }
}
