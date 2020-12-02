#![cfg_attr(target_arch = "spirv", no_std)]
#![feature(lang_items)]
#![feature(register_attr)]
#![register_attr(spirv)]

use spirv_std::glam::{Vec3, Vec4};
use spirv_std::{Input, Output};

#[allow(unused_attributes)]
#[spirv(vertex)]
pub fn main_vs(
    #[spirv(vertex_index)] vert_id: Input<i32>,
    #[spirv(position)] mut out_pos: Output<Vec4>,
    mut out_color: Output<Vec3>,
) {
    let vert_id = vert_id.load();
    out_pos.store(Vec4::new(
        (vert_id - 1) as f32,
        ((vert_id & 1) * 2 - 1) as f32,
        0.0,
        1.0,
    ));
    let color = if vert_id == 0 { Vec3::unit_y() }
        else if vert_id == 1 { Vec3::unit_z() }
        else if vert_id == 2 { Vec3::unit_x() }
        else { Vec3::one() };
    out_color.store(color);
}

#[allow(unused_attributes)]
#[spirv(fragment)]
pub fn main_fs(
    color: Input<Vec3>,
    mut output: Output<Vec4>
) {
    output.store(color.load().extend(1.0))
}

#[cfg(all(not(test), target_arch = "spirv"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[cfg(all(not(test), target_arch = "spirv"))]
#[lang = "eh_personality"]
extern "C" fn rust_eh_personality() {}
