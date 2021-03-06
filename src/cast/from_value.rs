//! 从 `Value` 到 Rust 对象的转换
//!
//! “Rust 传送值给 T10” 和 “T10 传送值给 Rust” 两种操作之间存在一个很大的不同之处：从 Rust 传给
//! T10 是个 “一锤子买卖”，而 T10 传给 Rust 时则需要考虑 Rust 函数返回之后，所传递 `Value`
//! 的 GC 标记。此外，当一个 `Rust` 函数有多个参数时，任何一个 `Value` 都可能失败，因此需要在一个
//! `Value` 的转换操作失败时回滚前面的所有操作。
//!
//! 因此，`FromValue` 操作实质上被拆分为两步：
//!   - 检查并更新 `Value` 的 GC 信息，并且获得一个用于恢复/回滚的 RAII 对象
//!   - 进行实际的数据拷贝/共享/转移

use std::mem::MaybeUninit;

use crate::data::{Value, GcInfo, GCINFO_READ_MASK, GCINFO_WRITE_MASK};
use crate::error::{TError, NullError, LifetimeError};
use crate::tyck::FFIAction;
use crate::tyck::base::StaticBase;
use crate::void::Void;

/// `GcInfoGuard` 是一个用于实现 `GcInfo` 更新的 RAII 装置
///
/// 当 Rust 函数成功返回时，像 `SharedWithRust` 或者 `MutSharedWithRust` 一类的状态需要恢复。
/// 当后面的参数转换失败时，像 `SharedWithRust`, `MutSharedWithRust` 或者 `MovedToRust`
/// 一类的操作需要撤销。
pub struct GcInfoGuard<'a> {
    /// 被管理的 `Value`
    value: &'a Value,
    /// 当 Rust 函数成功返回之后所要进行的 `GcInfo` 更新
    on_finish: Option<GcInfo>,
    /// 当函数调用失败时所要进行的 `GcInfo` 回滚
    on_yank: Option<GcInfo>
}

impl<'a> GcInfoGuard<'a> {
    #[inline] pub fn new(value: &'a Value, on_finish: GcInfo, on_yank: GcInfo) -> Self {
        Self {
            value,
            on_finish: Some(on_finish),
            on_yank: Some(on_yank)
        }
    }

    #[inline] pub fn no_action(value: &'a Value) -> Self {
        Self {
            value,
            on_finish: None,
            on_yank: None
        }
    }

    #[inline] pub fn finish(&mut self) {
        if let Some(on_finish) = self.on_finish {
            unsafe {
                self.value.set_gc_info(on_finish);
            }
        }
        let _ = self.on_yank.take();
    }
}

impl<'a> Drop for GcInfoGuard<'a> {
    #[inline] fn drop(&mut self) {
        if let Some(on_yank) = self.on_yank {
            unsafe {
                self.value.set_gc_info(on_yank);
            }
        }
    }
}

/// 在这一层 specialization 中特殊处理 `Option<T>` 和 `Value` 类型
///
/// ```compile_fail(E0597)
/// # use t10::data::{Value, ValueType};
/// # use t10::cast::from_value::FromValue;
/// # use t10::void::Void;
/// # fn main() {
/// let v = Value::null();
/// unsafe {
///     let x = <Void as FromValue<Option<&'static i64>>>::from_value(&v);
/// }
/// # }
/// ```
pub trait FromValue<'a, T> {
    fn lifetime_check(value: &'a Value) -> Result<GcInfoGuard<'a>, TError>;
    unsafe fn from_value(value: &'a Value) -> T;
}

/// 在这一层 specialization 中处理 `&T` 和 `&mut T`
pub trait FromValueL1<'a, T> {
    unsafe fn lifetime_check_l1(value: &'a Value) -> Result<GcInfoGuard<'a>, TError>;
    unsafe fn from_value_l1(value: &'a Value) -> T;
}

/// 在这一层 specialization 中特殊处理 `i64` 一类的值类型
pub trait FromValueL2<'a, T> {
    unsafe fn lifetime_check_l2(value: &'a Value) -> Result<GcInfoGuard<'a>, TError>;
    unsafe fn from_value_l2(value: &'a Value) -> T;
}

/// 在这一层 specialization 中区分处理 `Copy` 和 `!Copy` 类型
pub trait FromValueL3<'a, T> {
    unsafe fn lifetime_check_l3(value: &'a Value) -> Result<GcInfoGuard<'a>, TError>;
    unsafe fn from_value_l3(value: &'a Value, out: &mut MaybeUninit<T>);
}

impl<'a, T> FromValue<'a, T> for Void where Void: FromValueL1<'a, T> {
    default fn lifetime_check(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        if value.is_null() {
            Err(NullError().into())
        } else {
            unsafe { <Void as FromValueL1<'a, T>>::lifetime_check_l1(value) }
        }
    }

    default unsafe fn from_value(value: &'a Value) -> T {
        <Void as FromValueL1<'a, T>>::from_value_l1(value)
    }
}

impl<'a, T> FromValue<'a, Option<T>> for Void where Void: FromValueL1<'a, T> {
    fn lifetime_check(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        if value.is_null() {
            Ok(GcInfoGuard::no_action(value))
        } else {
            unsafe { <Void as FromValueL1<'a, T>>::lifetime_check_l1(value) }
        }
    }

    unsafe fn from_value(value: &'a Value) -> Option<T> {
        Some(<Void as FromValueL1<'a, T>>::from_value_l1(value))
    }
}

impl<'a> FromValue<'a, Value> for Void {
    fn lifetime_check(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        Ok(GcInfoGuard::no_action(value))
    }

    unsafe fn from_value(value: &'a Value) -> Value {
        *value
    }
}

impl<'a, T> FromValueL1<'a, T> for Void where Void: FromValueL2<'a, T> {
    #[inline] default unsafe fn lifetime_check_l1(value: &'a Value)
        -> Result<GcInfoGuard<'a>, TError>
    {
        debug_assert!(!value.is_null());
        <Void as FromValueL2<'a, T>>::lifetime_check_l2(value)
    }

    #[inline] default unsafe fn from_value_l1(value: &'a Value) -> T {
        debug_assert!(!value.is_null());
        <Void as FromValueL2<'a, T>>::from_value_l2(value)
    }
}

const INTO_REF_LIFETIMES: [GcInfo; 3] = [
    GcInfo::Owned,
    GcInfo::SharedToHost,
    GcInfo::SharedFromHost
];

impl<'a, T> FromValueL1<'a, &'a T> for Void where Void: FromValueL2<'a, T> {
    unsafe fn lifetime_check_l1(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        debug_assert!(!value.is_null());
        let actual = value.gc_info();
        match actual {
            GcInfo::Owned => {
                value.set_gc_info(GcInfo::SharedToHost);
                Ok(GcInfoGuard::new(value, GcInfo::Owned, GcInfo::Owned))
            },
            GcInfo::SharedToHost | GcInfo::SharedFromHost => {
                Ok(GcInfoGuard::no_action(value))
            },
            _ => Err(LifetimeError::new(&INTO_REF_LIFETIMES, FFIAction::Share, actual).into())
        }
    }

    unsafe fn from_value_l1(value: &'a Value) -> &'a T {
        debug_assert!(!value.is_null());
        value.as_ref()
    }
}

const INTO_MUT_REF_LIFETIMES: [GcInfo; 2] = [
    GcInfo::MutSharedFromHost,
    GcInfo::Owned
];
const GCINFO_RW_MASK: u8 = GCINFO_READ_MASK | GCINFO_WRITE_MASK;

impl<'a, T> FromValueL1<'a, &'a mut T> for Void where Void: FromValueL2<'a, T> {
    unsafe fn lifetime_check_l1(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        debug_assert!(!value.is_null());
        let actual = value.gc_info();
        if actual as u8 & GCINFO_WRITE_MASK != 0 {
            value.set_gc_info(GcInfo::from(actual as u8 & !GCINFO_RW_MASK));
            Ok(GcInfoGuard::new(value, actual, actual))
        } else {
            Err(LifetimeError::new(&INTO_MUT_REF_LIFETIMES, FFIAction::MutShare, actual).into())
        }
    }

    #[inline] unsafe fn from_value_l1(value: &'a Value) -> &'a mut T {
        debug_assert!(!value.is_null());
        value.as_mut()
    }
}

impl<'a, T> FromValueL2<'a, T> for Void where Void: FromValueL3<'a, T> {
    #[inline] default unsafe fn lifetime_check_l2(value: &'a Value)
        -> Result<GcInfoGuard<'a>, TError>
    {
        <Void as FromValueL3<'a, T>>::lifetime_check_l3(value)
    }

    #[inline] default unsafe fn from_value_l2(value: &'a Value) -> T {
        let mut ret: MaybeUninit<T> = MaybeUninit::uninit();
        <Void as FromValueL3<'a, T>>::from_value_l3(value, &mut ret);
        ret.assume_init()
    }
}

const VALUE_TYPE_LIFETIMES: [GcInfo; 4] = [
    GcInfo::Owned,
    GcInfo::SharedFromHost,
    GcInfo::SharedToHost,
    GcInfo::TempObject
];
impl<'a> FromValueL2<'a, i64> for Void {
    #[inline] unsafe fn lifetime_check_l2(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        let actual = value.gc_info();
        if actual as u8 & GCINFO_READ_MASK != 0 {
            Ok(GcInfoGuard::no_action(value))
        } else {
            Err(LifetimeError::new(&VALUE_TYPE_LIFETIMES, FFIAction::Copy, actual).into())
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline] unsafe fn from_value_l2(value: &'a Value) -> i64 {
        if value.is_value() {
            value.value_typed_data.inner.int
        } else {
            let mut ret: MaybeUninit<i64> = MaybeUninit::uninit();
            value.ptr.as_mut().unwrap_unchecked().move_out(
                &mut ret as *mut MaybeUninit<_> as *mut ()
            );
            ret.assume_init()
        }
    }

    #[cfg(debug_assertions)]
    #[inline] unsafe fn from_value_l2(value: &'a Value) -> i64 {
        if value.is_value() {
            value.value_typed_data.inner.int
        } else {
            let mut ret: MaybeUninit<i64> = MaybeUninit::uninit();
            value.ptr.as_mut().unwrap_unchecked().move_out_ck(
                &mut ret as *mut MaybeUninit<_> as *mut (),
                std::any::TypeId::of::<MaybeUninit<i64>>()
            );
            ret.assume_init()
        }
    }
}

const MOVE_TYPE_LIFETIMES: [GcInfo; 1] = [ GcInfo::Owned ];
impl<'a, T> FromValueL3<'a, T> for Void where Void: StaticBase<T> {
    #[inline] default unsafe fn lifetime_check_l3(value: &'a Value)
        -> Result<GcInfoGuard<'a>, TError>
    {
        let actual = value.gc_info();
        if actual == GcInfo::Owned {
            value.set_gc_info(GcInfo::MovedToHost);
            Ok(GcInfoGuard::new(value, GcInfo::MovedToHost, GcInfo::Owned))
        } else {
            Err(LifetimeError::new(&MOVE_TYPE_LIFETIMES, FFIAction::Move, actual).into())
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline] default unsafe fn from_value_l3(value: &'a Value, out: &mut MaybeUninit<T>) {
        value.ptr.as_mut().unwrap_unchecked().move_out(
            out as *mut MaybeUninit<_> as *mut ()
        );
    }

    #[cfg(debug_assertions)]
    #[inline] default unsafe fn from_value_l3(value: &'a Value, out: &mut MaybeUninit<T>) {
        value.ptr.as_mut().unwrap_unchecked().move_out_ck(
            out as *mut MaybeUninit<_> as *mut (),
            <Void as StaticBase<T>>::base_type_id()
        );
    }
}

impl<'a, T> FromValueL3<'a, T> for Void where Void: StaticBase<T>, T: Copy {
    unsafe fn lifetime_check_l3(value: &'a Value) -> Result<GcInfoGuard<'a>, TError> {
        let actual = value.gc_info();
        if actual as u8 & GCINFO_READ_MASK != 0 {
            Ok(GcInfoGuard::no_action(value))
        } else {
            Err(LifetimeError::new(&VALUE_TYPE_LIFETIMES, FFIAction::Copy, actual).into())
        }
    }

    unsafe fn from_value_l3(value: &'a Value, out: &mut MaybeUninit<T>) {
        out.write(value.as_ref::<T>().clone());
    }
}
