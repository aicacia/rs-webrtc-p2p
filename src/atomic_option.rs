use std::{
  ptr::null_mut,
  sync::atomic::{AtomicPtr, Ordering},
};

pub struct AtomicOption<T>(AtomicPtr<T>);

unsafe impl<T> Send for AtomicOption<T> {}
unsafe impl<T> Sync for AtomicOption<T> {}

impl<T> AtomicOption<T> {
  pub fn some(value: T) -> Self {
    Self(AtomicPtr::new(Box::into_raw(Box::new(value))))
  }

  pub const fn none() -> Self {
    Self(AtomicPtr::new(std::ptr::null_mut()))
  }

  pub fn store(&self, ordering: Ordering, value: T) {
    let new_ptr = Box::into_raw(Box::new(value));
    let prev_ptr = self.0.swap(new_ptr, ordering);
    if prev_ptr.is_null() {
      return;
    }
    unsafe {
      drop(Box::from_raw(prev_ptr));
    }
  }

  pub fn get_or_store(&self, ordering: Ordering, value: T) -> &T {
    let new_ptr = Box::into_raw(Box::new(value));
    let prev_ptr = self.0.swap(new_ptr, ordering);
    if !prev_ptr.is_null() {
      unsafe {
        drop(Box::from_raw(prev_ptr));
      }
    }
    unsafe { &*new_ptr }
  }

  pub fn get_or_store_with<F>(
    &self,
    set_ordering: Ordering,
    get_ordering: Ordering,
    value_fn: F,
  ) -> &T
  where
    F: Fn() -> T,
  {
    let mut ptr = null_mut::<T>();
    let prev_ptr = match self.0.fetch_update(set_ordering, get_ordering, |prev_ptr| {
      if prev_ptr.is_null() {
        ptr = Box::into_raw(Box::new(value_fn()));
        Some(ptr)
      } else {
        ptr = prev_ptr;
        None
      }
    }) {
      Ok(ptr) => ptr,
      Err(ptr) => ptr,
    };
    if !prev_ptr.is_null() {
      unsafe {
        drop(Box::from_raw(prev_ptr));
      }
    }
    unsafe { &*ptr }
  }

  pub fn as_ref(&self, ordering: Ordering) -> Option<&T> {
    let ptr = self.0.load(ordering);
    if ptr.is_null() {
      return None;
    }
    unsafe { Some(&*ptr) }
  }

  pub fn take(&self, ordering: Ordering) -> Option<T> {
    let ptr = self.0.swap(std::ptr::null_mut(), ordering);
    if ptr.is_null() {
      return None;
    }
    unsafe { Some(*Box::from_raw(ptr)) }
  }

  pub fn is_none(&self, ordering: Ordering) -> bool {
    self.as_ref(ordering).is_none()
  }

  pub fn is_some(&self, ordering: Ordering) -> bool {
    self.as_ref(ordering).is_some()
  }
}

impl<T> Drop for AtomicOption<T> {
  fn drop(&mut self) {
    let ptr = self.0.load(Ordering::SeqCst);
    if ptr.is_null() {
      return;
    }
    unsafe { drop(Box::from_raw(ptr)) };
  }
}
