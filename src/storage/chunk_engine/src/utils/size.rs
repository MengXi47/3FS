//! 以 byte 為單位的大小型別，crate 內所有容量、offset、chunk_size 都用它表示。
//!
//! - 單位一律是 2 的冪 (KiB = 1024)；常數名雖叫 `KB` / `MB`，實際是 KiB / MiB。
//! - `#[repr(C)]` 包一個 u64，記憶體佈局與 u64 相同，所以內含 `Size` 的 `UsedSize` 可以在
//!   `cxx.rs` 直接 transmute 成 `ffi::RawUsedSize` (全 u64 欄位)。
//! - 算術運算 (`+ - * / %`) 都是裸運算，不做溢位檢查；與 i32 / i64 互轉用 `as`，負數會繞回。
//!
//! 兩種文字格式：
//!
//! ```text
//!   Display ({})   exact, largest unit that divides evenly   524288  -> "512KiB"
//!                                                            1536    -> "1536B"
//!   Debug   ({:?}) approximate via around(), 2 decimals       524288  -> "0.50MiB"
//!                  (switches unit once value >= half of it)   1536    -> "1.50KiB"
//! ```

/// byte 數的 newtype；`repr(C)` 保證與 u64 同佈局 (FFI transmute 依賴這點)。
#[derive(Default, Copy, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
#[repr(C)]
pub struct Size(pub u64);

impl Size {
    // 以下常數都是 2 的冪單位：KB = 1024 B、MB = 1024 KB ...
    pub const B: Size = Size::byte(1);
    pub const KB: Size = Size::kibibyte(1);
    pub const MB: Size = Size::mebibyte(1);
    pub const GB: Size = Size::gibibyte(1);
    pub const TB: Size = Size::tebibyte(1);

    pub const fn new(v: u64) -> Size {
        Size(v)
    }

    pub const fn zero() -> Size {
        Size::new(0)
    }

    pub const fn byte(value: u64) -> Size {
        Size::new(value)
    }

    pub const fn kibibyte(value: u64) -> Size {
        Size::new(value << 10)
    }

    pub const fn mebibyte(value: u64) -> Size {
        Size::new(value << 20)
    }

    pub const fn gibibyte(value: u64) -> Size {
        Size::new(value << 30)
    }

    pub const fn tebibyte(value: u64) -> Size {
        Size::new(value << 40)
    }

    /// 近似表示 (Debug 用)：值達到某單位的一半就改用該單位，保留兩位小數，
    /// 例如 512B 顯示為 "0.50KiB"。
    pub fn around(&self) -> String {
        if self.0 == 0 {
            "0B".to_string()
        } else if *self * 2 >= Self::TB {
            format!("{:.2}TiB", (self.0 as f64 / Self::TB.0 as f64))
        } else if *self * 2 >= Self::GB {
            format!("{:.2}GiB", (self.0 as f64 / Self::GB.0 as f64))
        } else if *self * 2 >= Self::MB {
            format!("{:.2}MiB", (self.0 as f64 / Self::MB.0 as f64))
        } else if *self * 2 >= Self::KB {
            format!("{:.2}KiB", (self.0 as f64 / Self::KB.0 as f64))
        } else {
            format!("{}B", self.0)
        }
    }

    pub fn is_power_of_two(&self) -> bool {
        self.0.is_power_of_two()
    }

    pub fn next_power_of_two(&self) -> Size {
        Size(self.0.next_power_of_two())
    }

    pub fn trailing_zeros(&self) -> u32 {
        self.0.trailing_zeros()
    }
}

// 替每個整數型別 $t 一次實作：與 Size 互轉 (From)、比較 (PartialEq)、
// 雙向的 Add / Mul / Rem 與 AddAssign / MulAssign，讓 `Size * 256`、`64 * Size::KB`、
// `size == 0` 這類寫法成立。轉換一律用 `as`，不檢查範圍。
macro_rules! impl_trait_for_size {
    ($($t:ty),*) => {
        $(impl From<$t> for Size {
            fn from(value: $t) -> Self {
                Self::new(value as _)
            }
        }

        impl From<Size> for $t {
            fn from(val: Size) -> Self {
                val.0 as _
            }
        }

        impl PartialEq<$t> for Size {
            fn eq(&self, other: &$t) -> bool {
                self.0 == *other as u64
            }
        }

        impl PartialEq<Size> for $t {
            fn eq(&self, other: &Size) -> bool {
                *self as u64 == other.0
            }
        }

        impl std::ops::Add<$t> for Size {
            type Output = Size;

            fn add(self, rhs: $t) -> Self::Output {
                Size::new(self.0 + rhs as u64)
            }
        }

        impl std::ops::Add<Size> for $t {
            type Output = Size;

            fn add(self, rhs: Size) -> Self::Output {
                Size::new(self as u64 + rhs.0)
            }
        }

        impl std::ops::AddAssign<$t> for Size {
            fn add_assign(&mut self, rhs: $t) {
                self.0 += rhs as u64;
            }
        }

        impl std::ops::Mul<$t> for Size {
            type Output = Size;

            fn mul(self, rhs: $t) -> Self::Output {
                Size::new(self.0 * rhs as u64)
            }
        }

        impl std::ops::Mul<Size> for $t {
            type Output = Size;

            fn mul(self, rhs: Size) -> Self::Output {
                Size::new(self as u64 * rhs.0)
            }
        }

        impl std::ops::MulAssign<$t> for Size {
            fn mul_assign(&mut self, rhs: $t) {
                self.0 *= rhs as u64;
            }
        }

        impl std::ops::Rem<$t> for Size {
            type Output = Size;

            fn rem(self, rhs: $t) -> Self::Output {
                Size::new(self.0 % rhs as u64)
            }
        }

        impl std::ops::Rem<Size> for $t {
            type Output = Size;

            fn rem(self, rhs: Size) -> Self::Output {
                Size::new(self as u64 % rhs.0)
            }
        }
    )*
    };
}

impl_trait_for_size! {i32, i64, u32, u64, usize}

impl std::ops::Add<Self> for Size {
    type Output = Size;

    fn add(self, rhs: Self) -> Self::Output {
        Size::new(self.0 + rhs.0)
    }
}

impl std::ops::AddAssign<Self> for Size {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl std::ops::Sub<Self> for Size {
    type Output = Size;

    fn sub(self, rhs: Self) -> Self::Output {
        Size::new(self.0 - rhs.0)
    }
}

impl std::ops::Mul<Self> for Size {
    type Output = Size;

    fn mul(self, rhs: Self) -> Self::Output {
        Size::new(self.0 * rhs.0)
    }
}

impl std::ops::Div<Self> for Size {
    type Output = Size;

    fn div(self, rhs: Self) -> Self::Output {
        Size::new(self.0 / rhs.0)
    }
}

impl std::ops::Rem<Self> for Size {
    type Output = Size;

    fn rem(self, rhs: Self) -> Self::Output {
        Size::new(self.0 % rhs.0)
    }
}

/// 精確表示：取能整除的最大單位，否則退回 byte。`Clusters` 的子目錄名
/// (`64KiB/`、`512KiB/`... 見 `alloc/allocators.rs` 的 `create`) 就是用它產生的，
/// 改輸出格式會讓既有資料目錄對不上。
impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 {
            write!(f, "0B")
        } else if *self % Self::TB == 0 {
            write!(f, "{}TiB", (*self / Self::TB).0)
        } else if *self % Self::GB == 0 {
            write!(f, "{}GiB", (*self / Self::GB).0)
        } else if *self % Self::MB == 0 {
            write!(f, "{}MiB", (*self / Self::MB).0)
        } else if *self % Self::KB == 0 {
            write!(f, "{}KiB", (*self / Self::KB).0)
        } else {
            write!(f, "{}B", self.0)
        }
    }
}

/// 近似表示，委派給 `around()`；tracing log 用 `{:?}` 印 Size 時看到的是這個格式。
impl std::fmt::Debug for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.around())
    }
}

#[cfg(test)]
mod tests {
    // 驗證單位建構、Display / Debug 兩種格式，以及與整數的混合運算
    #[test]
    fn test_size() {
        use super::Size;

        let size = Size::zero();
        assert_eq!(size, Size::new(0));
        assert_eq!(size.to_string(), "0B".to_string());

        let size = Size::kibibyte(64);
        assert_eq!(size, Size::new(65536));
        assert_eq!(size.to_string(), "64KiB".to_string());

        let size: Size = Size::MB * 23;
        assert_eq!(size, Size::new(23 << 20));
        assert_eq!(size.to_string(), "23MiB".to_string());

        let size: Size = 233 * Size::GB;
        assert_eq!(size, Size::new(233 << 30));
        assert_eq!(size.to_string(), "233GiB".to_string());

        assert_eq!(format!("{}", Size::zero()), "0B".to_string());
        assert_eq!(format!("{}", Size::byte(233)), "233B".to_string());
        assert_eq!(format!("{}", Size::byte(512)), "512B".to_string());
        assert_eq!(format!("{}", Size::kibibyte(512)), "512KiB".to_string());
        assert_eq!(format!("{}", Size::mebibyte(512)), "512MiB".to_string());
        assert_eq!(format!("{}", Size::gibibyte(512)), "512GiB".to_string());
        assert_eq!(format!("{}", Size::tebibyte(512)), "512TiB".to_string());

        assert_eq!(format!("{:?}", Size::zero()), "0B".to_string());
        assert_eq!(format!("{:?}", Size::byte(233)), "233B".to_string());
        assert_eq!(format!("{:?}", Size::byte(512)), "0.50KiB".to_string());
        assert_eq!(format!("{:?}", Size::kibibyte(512)), "0.50MiB".to_string());
        assert_eq!(format!("{:?}", Size::mebibyte(512)), "0.50GiB".to_string());
        assert_eq!(format!("{:?}", Size::gibibyte(512)), "0.50TiB".to_string());
        assert_eq!(format!("{:?}", Size::tebibyte(512)), "512.00TiB".to_owned());

        let r = rand::random::<u64>() % 1024;
        assert_eq!(0 + Size::kibibyte(r), Size::from(r << 10));
        assert_eq!(Size::mebibyte(r) + 0, Size::from(r << 20));
        assert_eq!(1 * Size::gibibyte(r), Size::from(r << 30));
        assert_eq!(Size::tebibyte(r) * 1, Size::from(r << 40));

        assert_eq!(Size::KB * Size::KB, Size::MB);
        let mut size = Size::B;
        size *= 1024;
        assert_eq!(size, Size::KB);
        assert_eq!(size % 1000, 24);

        assert_eq!(Size::KB + Size::KB, Size::kibibyte(2));
        assert_eq!(Size::KB % 1000, Size(24));
    }
}
