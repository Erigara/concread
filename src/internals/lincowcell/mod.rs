//! A CowCell with linear drop behaviour
//!
//! YOU SHOULD NOT USE THIS TYPE! Normaly concurrent cells do NOT require the linear dropping
//! behaviour that this implements, and it will only make your application
//! worse for it. Consider `CowCell` and `EbrCell` instead.

/*
 * The reason this exists is for protecting the major concurrently readable structures
 * that can corrupt if intermediate transactions are removed early. Effectively what we
 * need to create is:
 *
 * [ A ] -> [ B ] -> [ C ] -> [ Write Head ]
 *   ^        ^        ^
 *   read     read     read
 *
 * This way if we drop the reader on B:
 *
 * [ A ] -> [ B ] -> [ C ] -> [ Write Head ]
 *   ^                 ^
 *   read              read
 *
 * Notice that A is not dropped. It's only when A is dropped:
 *
 * [ A ] -> [ B ] -> [ C ] -> [ Write Head ]
 *                     ^
 *                     read
 *
 * [ X ] -> [ B ] -> [ C ] -> [ Write Head ]
 *                     ^
 *                     read
 * [ X ] -> [ X ] -> [ C ] -> [ Write Head ]
 *                     ^
 *                     read
 *
 *                   [ C ] -> [ Write Head ]
 *                     ^
 *                     read
 *
 * At this point we drop A and B. To achieve this we need to consider that:
 * - If WriteHead is dropped, C continues to live.
 * - If A/B are dropped, we don't affect C.
 * - Everything is dropped in order until a read txn exists.
 * - When we drop the main structure, no readers can exist.
 * - A writer must be able to commit to a stable location.
 *
 *
 *   T        T        T
 * [ A ] -> [ B ] -> [ C ] -> [ Write Head ]
 *   ^        ^        ^
 *   RRR      RR       R
 *
 *
 * As the write head proceeds, it must be able to interact with past versions to commit
 * garbage that is "last seen" in the formers generation.
 *
 */

use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};

/// Do not implement this. You don't need this negativity in your life.
pub trait LinCowCellCapable<R, U> {
    /// Create the first reader snapshot for a new instance.
    fn create_reader(&self) -> R;

    /// Create a writer that may be rolled back.
    fn create_writer(&self) -> U;

    /// Given the current active reader, and the writer to commit, update our
    /// main structure as mut self, and our previously linear generations based on
    /// what was updated.
    fn pre_commit(&mut self, new: U, prev: &R) -> R;
}

#[derive(Debug)]
/// A concurrently readable cell with linearised drop behaviour.
pub struct LinCowCell<T, R, U> {
    updater: PhantomData<U>,
    write: Mutex<T>,
    active: Mutex<Arc<LinCowCellInner<R>>>,
}

#[derive(Debug)]
/// A write txn over a linear cell.
pub struct LinCowCellWriteTxn<'a, T: 'a, R, U> {
    // This way we know who to contact for updating our data ....
    caller: &'a LinCowCell<T, R, U>,
    guard: MutexGuard<'a, T>,
    work: U,
}

#[derive(Debug)]
struct LinCowCellInner<R> {
    // This gives the chain effect.
    pin: Mutex<Option<Arc<LinCowCellInner<R>>>>,
    data: R,
}

#[derive(Debug)]
/// A read txn over a linear cell.
pub struct LinCowCellReadTxn<'a, T: 'a, R, U> {
    // We must outlive the root
    _caller: &'a LinCowCell<T, R, U>,
    // We pin the current version.
    work: Arc<LinCowCellInner<R>>,
}

impl<R> LinCowCellInner<R> {
    pub fn new(data: R) -> Self {
        LinCowCellInner {
            pin: Mutex::new(None),
            data,
        }
    }
}

impl<T, R, U> LinCowCell<T, R, U>
where
    T: LinCowCellCapable<R, U>,
{
    /// Create a new linear 🐄 cell.
    pub fn new(data: T) -> Self {
        let r = data.create_reader();
        LinCowCell {
            updater: PhantomData,
            write: Mutex::new(data),
            active: Mutex::new(Arc::new(LinCowCellInner::new(r))),
        }
    }

    /// Begin a read txn
    pub fn read(&self) -> LinCowCellReadTxn<T, R, U> {
        let rwguard = self.active.lock().unwrap();
        LinCowCellReadTxn {
            _caller: self,
            // inc the arc.
            work: rwguard.clone(),
        }
    }

    /// Begin a write txn
    pub fn write(&self) -> LinCowCellWriteTxn<T, R, U> {
        /* Take the exclusive write lock first */
        let write_guard = self.write.lock().unwrap();
        /* Now take a ro-txn to get the data copied */
        // let active_guard = self.active.lock();
        /* This copies the data */
        let work: U = (*write_guard).create_writer();
        /* Now build the write struct */
        LinCowCellWriteTxn {
            caller: self,
            guard: write_guard,
            work,
        }
    }

    /// Attempt a write txn
    pub fn try_write(&self) -> Option<LinCowCellWriteTxn<T, R, U>> {
        self.write.try_lock().ok().map(|write_guard| {
            /* This copies the data */
            let work: U = (*write_guard).create_writer();
            /* Now build the write struct */
            LinCowCellWriteTxn {
                caller: self,
                guard: write_guard,
                work,
            }
        })
    }

    fn commit(&self, write: LinCowCellWriteTxn<T, R, U>) {
        // Destructure our writer.
        let LinCowCellWriteTxn {
            // This is self.
            caller: _caller,
            mut guard,
            work,
        } = write;

        // Get the previous generation.
        let mut rwguard = self.active.lock().unwrap();
        // Start to setup for the commit.
        let newdata = guard.pre_commit(work, &rwguard.data);

        let new_inner = Arc::new(LinCowCellInner::new(newdata));
        {
            // This modifies the next pointer of the existing read txns
            let mut rwguard_inner = rwguard.pin.lock().unwrap();
            // Create the arc pointer to our new data
            // add it to the last value
            *rwguard_inner = Some(new_inner.clone());
        }
        // now over-write the last value in the mutex.
        *rwguard = new_inner;
    }
}

impl<'a, T, R, U> Deref for LinCowCellReadTxn<'a, T, R, U> {
    type Target = R;

    #[inline]
    fn deref(&self) -> &R {
        &self.work.data
    }
}

impl<'a, T, R, U> AsRef<R> for LinCowCellReadTxn<'a, T, R, U> {
    #[inline]
    fn as_ref(&self) -> &R {
        &self.work.data
    }
}

impl<'a, T, R, U> LinCowCellWriteTxn<'a, T, R, U>
where
    T: LinCowCellCapable<R, U>,
{
    #[inline]
    /// Get the mutable inner of this type
    pub fn get_mut(&mut self) -> &mut U {
        &mut self.work
    }

    /// Commit the active changes.
    pub fn commit(self) {
        /* Write our data back to the LinCowCell */
        self.caller.commit(self);
    }
}

impl<'a, T, R, U> Deref for LinCowCellWriteTxn<'a, T, R, U> {
    type Target = U;

    #[inline]
    fn deref(&self) -> &U {
        &self.work
    }
}

impl<'a, T, R, U> DerefMut for LinCowCellWriteTxn<'a, T, R, U> {
    #[inline]
    fn deref_mut(&mut self) -> &mut U {
        &mut self.work
    }
}

impl<'a, T, R, U> AsRef<U> for LinCowCellWriteTxn<'a, T, R, U> {
    #[inline]
    fn as_ref(&self) -> &U {
        &self.work
    }
}

impl<'a, T, R, U> AsMut<U> for LinCowCellWriteTxn<'a, T, R, U> {
    #[inline]
    fn as_mut(&mut self) -> &mut U {
        &mut self.work
    }
}

#[cfg(test)]
mod tests {
    use super::LinCowCell;
    use super::LinCowCellCapable;
    use crossbeam_utils::thread::scope;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct TestData {
        x: i64,
    }

    #[derive(Debug)]
    struct TestDataReadTxn {
        x: i64,
    }

    #[derive(Debug)]
    struct TestDataWriteTxn {
        x: i64,
    }

    impl LinCowCellCapable<TestDataReadTxn, TestDataWriteTxn> for TestData {
        fn create_reader(&self) -> TestDataReadTxn {
            TestDataReadTxn { x: self.x }
        }

        fn create_writer(&self) -> TestDataWriteTxn {
            TestDataWriteTxn { x: self.x }
        }

        fn pre_commit(
            &mut self,
            new: TestDataWriteTxn,
            _prev: &TestDataReadTxn,
        ) -> TestDataReadTxn {
            // Update self if needed.
            self.x = new.x;
            // return a new reader.
            TestDataReadTxn { x: new.x }
        }
    }

    #[test]
    fn test_simple_create() {
        let data = TestData { x: 0 };
        let cc = LinCowCell::new(data);

        let cc_rotxn_a = cc.read();
        println!("cc_rotxn_a -> {:?}", cc_rotxn_a);
        assert_eq!(cc_rotxn_a.work.data.x, 0);

        {
            /* Take a write txn */
            let mut cc_wrtxn = cc.write();
            println!("cc_wrtxn -> {:?}", cc_wrtxn);
            assert_eq!(cc_wrtxn.work.x, 0);
            assert_eq!(cc_wrtxn.as_ref().x, 0);
            {
                let mut_ptr = cc_wrtxn.get_mut();
                /* Assert it's 0 */
                assert_eq!(mut_ptr.x, 0);
                mut_ptr.x = 1;
                assert_eq!(mut_ptr.x, 1);
            }
            // Check we haven't mutated the old data.
            assert_eq!(cc_rotxn_a.work.data.x, 0);
        }
        // The writer is dropped here. Assert no changes.
        assert_eq!(cc_rotxn_a.work.data.x, 0);
        {
            /* Take a new write txn */
            let mut cc_wrtxn = cc.write();
            println!("cc_wrtxn -> {:?}", cc_wrtxn);
            assert_eq!(cc_wrtxn.work.x, 0);
            assert_eq!(cc_wrtxn.as_ref().x, 0);
            {
                let mut_ptr = cc_wrtxn.get_mut();
                /* Assert it's 0 */
                assert_eq!(mut_ptr.x, 0);
                mut_ptr.x = 2;
                assert_eq!(mut_ptr.x, 2);
            }
            // Check we haven't mutated the old data.
            assert_eq!(cc_rotxn_a.work.data.x, 0);
            // Now commit
            cc_wrtxn.commit();
        }
        // Should not be percieved by the old txn.
        assert_eq!(cc_rotxn_a.work.data.x, 0);
        let cc_rotxn_c = cc.read();
        // Is visible to the new one though.
        assert_eq!(cc_rotxn_c.work.data.x, 2);
    }

    // == mt tests ==

    fn mt_writer(cc: &LinCowCell<TestData, TestDataReadTxn, TestDataWriteTxn>) {
        let mut last_value: i64 = 0;
        while last_value < 500 {
            let mut cc_wrtxn = cc.write();
            {
                let mut_ptr = cc_wrtxn.get_mut();
                assert!(mut_ptr.x >= last_value);
                last_value = mut_ptr.x;
                mut_ptr.x = mut_ptr.x + 1;
            }
            cc_wrtxn.commit();
        }
    }

    fn rt_writer(cc: &LinCowCell<TestData, TestDataReadTxn, TestDataWriteTxn>) {
        let mut last_value: i64 = 0;
        while last_value < 500 {
            let cc_rotxn = cc.read();
            {
                assert!(cc_rotxn.work.data.x >= last_value);
                last_value = cc_rotxn.work.data.x;
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_multithread_create() {
        let start = time::Instant::now();
        // Create the new cowcell.
        let data = TestData { x: 0 };
        let cc = LinCowCell::new(data);

        assert!(scope(|scope| {
            let cc_ref = &cc;

            let _readers: Vec<_> = (0..7)
                .map(|_| {
                    scope.spawn(move |_| {
                        rt_writer(cc_ref);
                    })
                })
                .collect();

            let _writers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(move |_| {
                        mt_writer(cc_ref);
                    })
                })
                .collect();
        })
        .is_ok());

        let end = time::Instant::now();
        print!("Arc MT create :{:?} ", end - start);
    }

    static GC_COUNT: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Clone)]
    struct TestGcWrapper<T> {
        data: T,
    }

    #[derive(Debug)]
    struct TestGcWrapperReadTxn<T> {
        _data: T,
    }

    #[derive(Debug)]
    struct TestGcWrapperWriteTxn<T> {
        data: T,
    }

    impl<T: Clone> LinCowCellCapable<TestGcWrapperReadTxn<T>, TestGcWrapperWriteTxn<T>>
        for TestGcWrapper<T>
    {
        fn create_reader(&self) -> TestGcWrapperReadTxn<T> {
            TestGcWrapperReadTxn {
                _data: self.data.clone(),
            }
        }

        fn create_writer(&self) -> TestGcWrapperWriteTxn<T> {
            TestGcWrapperWriteTxn {
                data: self.data.clone(),
            }
        }

        fn pre_commit(
            &mut self,
            new: TestGcWrapperWriteTxn<T>,
            _prev: &TestGcWrapperReadTxn<T>,
        ) -> TestGcWrapperReadTxn<T> {
            // Update self if needed.
            self.data = new.data.clone();
            // return a new reader.
            TestGcWrapperReadTxn {
                _data: self.data.clone(),
            }
        }
    }

    impl<T> Drop for TestGcWrapperReadTxn<T> {
        fn drop(&mut self) {
            // Add to the atomic counter ...
            GC_COUNT.fetch_add(1, Ordering::Release);
        }
    }

    fn test_gc_operation_thread(
        cc: &LinCowCell<TestGcWrapper<i64>, TestGcWrapperReadTxn<i64>, TestGcWrapperWriteTxn<i64>>,
    ) {
        while GC_COUNT.load(Ordering::Acquire) < 50 {
            // thread::sleep(std::time::Duration::from_millis(200));
            {
                let mut cc_wrtxn = cc.write();
                {
                    let mut_ptr = cc_wrtxn.get_mut();
                    mut_ptr.data = mut_ptr.data + 1;
                }
                cc_wrtxn.commit();
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_gc_operation() {
        GC_COUNT.store(0, Ordering::Release);
        let data = TestGcWrapper { data: 0 };
        let cc = LinCowCell::new(data);

        assert!(scope(|scope| {
            let cc_ref = &cc;
            let _writers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(move |_| {
                        test_gc_operation_thread(cc_ref);
                    })
                })
                .collect();
        })
        .is_ok());

        assert!(GC_COUNT.load(Ordering::Acquire) >= 50);
    }
}

#[cfg(test)]
mod tests_linear {
    use super::LinCowCell;
    use super::LinCowCellCapable;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static GC_COUNT: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Clone)]
    struct TestGcWrapper<T> {
        data: T,
    }

    #[derive(Debug)]
    struct TestGcWrapperReadTxn<T> {
        _data: T,
    }

    #[derive(Debug)]
    struct TestGcWrapperWriteTxn<T> {
        data: T,
    }

    impl<T: Clone> LinCowCellCapable<TestGcWrapperReadTxn<T>, TestGcWrapperWriteTxn<T>>
        for TestGcWrapper<T>
    {
        fn create_reader(&self) -> TestGcWrapperReadTxn<T> {
            TestGcWrapperReadTxn {
                _data: self.data.clone(),
            }
        }

        fn create_writer(&self) -> TestGcWrapperWriteTxn<T> {
            TestGcWrapperWriteTxn {
                data: self.data.clone(),
            }
        }

        fn pre_commit(
            &mut self,
            new: TestGcWrapperWriteTxn<T>,
            _prev: &TestGcWrapperReadTxn<T>,
        ) -> TestGcWrapperReadTxn<T> {
            // Update self if needed.
            self.data = new.data.clone();
            // return a new reader.
            TestGcWrapperReadTxn {
                _data: self.data.clone(),
            }
        }
    }

    impl<T> Drop for TestGcWrapperReadTxn<T> {
        fn drop(&mut self) {
            // Add to the atomic counter ...
            GC_COUNT.fetch_add(1, Ordering::Release);
        }
    }

    /*
     * This tests an important property of the lincowcell over the cow cell
     * that read txns are dropped *in order*.
     */
    #[test]
    fn test_gc_operation_linear() {
        GC_COUNT.store(0, Ordering::Release);
        assert!(GC_COUNT.load(Ordering::Acquire) == 0);
        let data = TestGcWrapper { data: 0 };
        let cc = LinCowCell::new(data);

        // Open a read A.
        let cc_rotxn_a = cc.read();
        let cc_rotxn_a_2 = cc.read();
        // open a write, change and commit
        {
            let mut cc_wrtxn = cc.write();
            {
                let mut_ptr = cc_wrtxn.get_mut();
                mut_ptr.data = mut_ptr.data + 1;
            }
            cc_wrtxn.commit();
        }
        // open a read B.
        let cc_rotxn_b = cc.read();
        // open a write, change and commit
        {
            let mut cc_wrtxn = cc.write();
            {
                let mut_ptr = cc_wrtxn.get_mut();
                mut_ptr.data = mut_ptr.data + 1;
            }
            cc_wrtxn.commit();
        }
        // open a read C
        let cc_rotxn_c = cc.read();

        assert!(GC_COUNT.load(Ordering::Acquire) == 0);

        // drop B
        drop(cc_rotxn_b);

        // gc count should be 0.
        assert!(GC_COUNT.load(Ordering::Acquire) == 0);

        // drop C
        drop(cc_rotxn_c);

        // gc count should be 0
        assert!(GC_COUNT.load(Ordering::Acquire) == 0);

        // Drop the second A, should not trigger yet.
        drop(cc_rotxn_a_2);
        assert!(GC_COUNT.load(Ordering::Acquire) == 0);

        // drop A
        drop(cc_rotxn_a);

        // gc count should be 2 (A + B, C is still live)
        assert!(GC_COUNT.load(Ordering::Acquire) == 2);
    }
}
