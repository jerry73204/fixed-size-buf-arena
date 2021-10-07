use crossbeam::queue::SegQueue;
use std::{
    iter,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    sync::Arc,
};
use tokio::sync::Notify;

pub struct Arena<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Arena<T>
where
    T: Default,
{
    pub fn new(num_buf: usize, buf_len: usize) -> Self {
        unsafe {
            let segment_len = num_buf * buf_len;
            assert!(segment_len > 0);

            let segment_len = num_buf * buf_len;
            let ptr = {
                let mut segment = Vec::with_capacity(segment_len);
                (0..segment_len).for_each(|_| {
                    segment.push(T::default());
                });
                let segment = segment.into_boxed_slice();
                NonNull::new_unchecked(Box::into_raw(segment))
            };

            let free_list = SegQueue::new();
            iter::successors(Some(0), |&prev| {
                let next = prev + buf_len;
                (next < segment_len).then(|| next)
            })
            .for_each(|index| {
                free_list.push(index);
            });

            Self {
                inner: Arc::new(Inner {
                    buf_len,
                    free_list,
                    segment: ptr,
                    not_full: Notify::new(),
                }),
            }
        }
    }

    pub fn try_take(&self) -> Option<Ref<T>> {
        let Inner { ref free_list, .. } = *self.inner;

        free_list.pop().map(|index| {
            let mut slice = Ref {
                inner: self.inner.clone(),
                index,
            };
            slice.iter_mut().for_each(|elem| {
                *elem = T::default();
            });
            slice
        })
    }

    pub async fn take(&self) -> Ref<T> {
        loop {
            let Inner {
                ref free_list,
                ref not_full,
                ..
            } = *self.inner;

            match free_list.pop() {
                Some(index) => {
                    let mut slice = Ref {
                        inner: self.inner.clone(),
                        index,
                    };
                    slice.iter_mut().for_each(|elem| {
                        *elem = T::default();
                    });
                    break slice;
                }
                None => {
                    not_full.notified().await;
                }
            }
        }
    }
}

unsafe impl<T> Send for Arena<T> where T: Send {}
unsafe impl<T> Sync for Arena<T> where T: Send {}

struct Inner<T> {
    buf_len: usize,
    free_list: SegQueue<usize>,
    segment: NonNull<[T]>,
    not_full: Notify,
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.segment.as_mut()));
        }
    }
}

unsafe impl<T> Send for Inner<T> where T: Send {}

unsafe impl<T> Sync for Inner<T> where T: Send {}

pub struct Ref<T> {
    inner: Arc<Inner<T>>,
    index: usize,
}

unsafe impl<T> Send for Ref<T> where T: Send {}

impl<T> Deref for Ref<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        unsafe {
            let index = self.index;
            let segment = self.inner.segment;
            let buf_len = self.inner.buf_len;

            &segment.as_ref()[index..(index + buf_len)]
        }
    }
}

impl<T> DerefMut for Ref<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let index = self.index;
            let mut segment = self.inner.segment;
            let buf_len = self.inner.buf_len;

            &mut segment.as_mut()[index..(index + buf_len)]
        }
    }
}

impl<T> Drop for Ref<T> {
    fn drop(&mut self) {
        let Inner {
            free_list,
            not_full,
            ..
        } = &*self.inner;
        free_list.push(self.index);
        not_full.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{prelude::*, stream};
    use par_stream::prelude::*;
    use rand::prelude::*;
    use std::time::Instant;

    #[async_std::test]
    async fn sanity_test() {
        let mut rng = rand::thread_rng();
        let arena = Arc::new(Arena::<u8>::new(2, 8));

        let mut s1 = arena.take().await;
        let mut s2 = arena.take().await;

        assert!(arena.try_take().is_none());

        rng.fill(&mut *s1);
        rng.fill(&mut *s2);
        drop(s1);
        drop(s2);

        let s1 = arena.take().await;
        let s2 = arena.take().await;
        assert!(
            s1.iter().all(|&val| val == Default::default())
                && s2.iter().all(|&val| val == Default::default())
        );
    }

    #[async_std::test]
    async fn benchmark() {
        const NUM_TASKS: usize = 1_000_000;
        const SLICE_LEN: usize = 64;

        let instant = Instant::now();
        let arena = Arc::new(Arena::<u8>::new(32, SLICE_LEN));
        eprintln!("init arena:\t{:?}", instant.elapsed());

        let instant = Instant::now();
        {
            stream::iter(0..NUM_TASKS)
                .par_for_each(None, move |_| {
                    let arena = arena.clone();

                    async move {
                        let mut slice = arena.take().await;
                        let mut rng = rand::thread_rng();
                        rng.fill(&mut *slice);
                    }
                })
                .await;
        }
        eprintln!("arena:\t{:?}", instant.elapsed());

        let instant = Instant::now();
        {
            stream::iter(0..NUM_TASKS)
                .par_for_each(None, move |_| async move {
                    let mut slice = vec![0; SLICE_LEN];
                    let mut rng = rand::thread_rng();
                    rng.fill(&mut *slice);
                })
                .await;
        }
        eprintln!("std:\t{:?}", instant.elapsed());
    }
}
