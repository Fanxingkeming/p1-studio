#[allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    dead_code,
    clippy::all
)]
pub mod Microsoft {
    pub mod Web {
        pub mod WebView2 {
            pub mod Win32 {
                mod windows_core {
                    // Local fork: instead of declaring an `extern` block (which
                    // would put WebView2Loader.dll in the import table and force
                    // it to sit next to the .exe), define the entry point as a
                    // Rust function that resolves a function pointer from the
                    // loader carried inside this binary. See src/loader.rs.
                    macro_rules! link_webview2 {
                        ($library:literal $abi:literal fn $name:ident($($arg:ident : $ty:ty),*) -> $ret:ty) => (
                            unsafe fn $name($($arg: $ty),*) -> $ret {
                                type Sig = unsafe extern $abi fn($($ty),*) -> $ret;
                                static CACHED: ::std::sync::OnceLock<Sig> = ::std::sync::OnceLock::new();
                                let entry: Sig = *CACHED.get_or_init(|| {
                                    let p = crate::loader::symbol(stringify!($name))
                                        .unwrap_or_else(|| panic!(
                                            "WebView2 加载失败：无法定位 {}（请在诊断页查看 WebView2 运行时状态）",
                                            stringify!($name),
                                        ));
                                    // SAFETY: the export has exactly this ABI.
                                    unsafe { ::std::mem::transmute::<*const (), Sig>(p) }
                                });
                                // `entry` is a plain function pointer, not this
                                // function, so there is no recursion here.
                                unsafe { entry($($arg),*) }
                            }
                        )
                    }

                    pub(crate) use ::windows_core::*;
                    pub(crate) use link_webview2 as link;
                }

                include!("bindings.rs");
            }
        }
    }
}

pub mod declared_interfaces;
pub mod loader;

#[cfg(test)]
mod test {
    use windows_core::w;

    use crate::Microsoft::Web::WebView2::Win32::*;

    #[test]
    fn compare_eq() {
        let mut result = 1;
        unsafe { CompareBrowserVersions(w!("1.0.0"), w!("1.0.0"), &mut result) }.unwrap();
        assert_eq!(0, result);
    }

    #[test]
    fn compare_lt() {
        let mut result = 0;
        unsafe { CompareBrowserVersions(w!("1.0.0"), w!("1.0.1"), &mut result) }.unwrap();
        assert_eq!(-1, result);
    }

    #[test]
    fn compare_gt() {
        let mut result = 0;
        unsafe { CompareBrowserVersions(w!("2.0.0"), w!("1.0.1"), &mut result) }.unwrap();
        assert_eq!(1, result);
    }
}
