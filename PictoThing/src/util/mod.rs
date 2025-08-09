#[macro_export] macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_DRAM : static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_DRAM.uninit().write(($val));
        x
    }};
}
#[macro_export] macro_rules! mk_static_dram2 {
    ($t:ty,$val:expr) => {{
        unsafe {
            #[link_section =".dram2_uninit"]
            static mut STATIC_DRAM2 : MaybeUninit<$t> = MaybeUninit::uninit();
            #[deny(unused_attributes)]
            let x = STATIC_DRAM2.write(($val));
            x
        }
    }};
}

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

pub fn get_file(name: &str) -> Option<&'static [u8]> {
    for (n, data) in FILES {
        if *n == name {
            return Some(*data);
        }
    }
    None
}
