#![feature(test)]
//#[cfg(feature = "benchmark-internals")]

extern crate test;

// use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

// use rustorrent::cache_line::CacheAligned;

// use test::Bencher;

// pub fn acquire_free_node_u8(bitfields: &[CacheAligned<AtomicU8>]) -> Option<usize> {
//     for byte in &bitfields[..] {
//         let mut bitfield = byte.load(Ordering::Relaxed);

//         let mut index_free = bitfield.trailing_zeros();

//         if index_free == 8 {
//             continue;
//         }

//         // Bitfield where we clear the bit of the free node to mark
//         // it as non-free
//         let mut new_bitfield = bitfield & !(1 << index_free);

//         while let Err(x) = byte.compare_exchange_weak(
//             bitfield, new_bitfield, Ordering::SeqCst, Ordering::Relaxed
//         ) {
//             bitfield = x;
//             index_free = bitfield.trailing_zeros();

//             if index_free == 8 {
//                 continue
//             }

//             new_bitfield = bitfield & !(1 << index_free);
//         }

//         return Some(index_free as usize);
//     }

//     None
// }

// pub fn acquire_free_node_u32(a_bitfield: &CacheAligned<AtomicU32>) -> Option<usize> {
//     let mut bitfield = a_bitfield.load(Ordering::Relaxed);

//     let mut index_free = bitfield.trailing_zeros();

//     if index_free == 32 {
//         return None;
//     }

//     // Bitfield where we clear the bit of the free node to mark
//     // it as non-free
//     let mut new_bitfield = bitfield & !(1 << index_free);

//     while let Err(x) = a_bitfield.compare_exchange_weak(
//         bitfield, new_bitfield, Ordering::SeqCst, Ordering::Relaxed
//     ) {
//         bitfield = x;
//         index_free = bitfield.trailing_zeros();

//         if index_free == 32 {
//             return None;
//         }

//         new_bitfield = bitfield & !(1 << index_free);
//     }

//     Some(index_free as usize)
// }

// #[bench]
// fn bitfield_x100_u8(b: &mut Bencher) {
//     let bitfields: [CacheAligned<AtomicU8>; 4] = [
//         CacheAligned::new(AtomicU8::new(0)),
//         CacheAligned::new(AtomicU8::new(0)),
//         CacheAligned::new(AtomicU8::new(0)),
//         CacheAligned::new(AtomicU8::new(!0)),
//     ];

//     b.iter(|| {
//         for _ in 0..100 {
//             assert!(acquire_free_node_u8(&bitfields[..]).is_some());
//             bitfields[1].store(!1, Ordering::Relaxed);
//         }
//     });
// }

// #[bench]
// fn bitfield_x100_u32(b: &mut Bencher) {
//     let bitfield = CacheAligned::new(AtomicU32::new(0xFF));

//     b.iter(|| {
//         for _ in 0..100 {
//             assert!(acquire_free_node_u32(&bitfield).is_some());
//             bitfield.store(!1, Ordering::Relaxed);
//         }
//     });
// }

use shared_arena::{SharedArena, Arena, Pool};
//use rustorrent::memory_pool::{Arena, SharedArena, ArenaBox, Pool};

#[allow(dead_code)]
#[derive(Copy, Clone)]
struct MyStruct {
    a: Option<usize>,
    b: &'static str,
    c: usize,
    // d: [usize; 64]
}

impl Default for MyStruct {
    fn default() -> MyStruct {
        MyStruct {
            a: None,
            b: "hello",
            c: 90909,
            // d: unsafe { std::mem::zeroed() }
        }
    }
}

//https://chromium.googlesource.com/chromiumos/platform/crosvm/+/refs/heads/master/data_model/src/volatile_memory.rs

// use std::mem::ManuallyDrop;

// #[bench]
// fn shared_arena(b: &mut Bencher) {
//     let arena = SharedArena::<MyStruct>::with_capacity(200000000);
//     let obj = MyStruct::default();

//     b.iter(|| {
//         ManuallyDrop::new(arena.alloc(MyStruct::default()))

//         // arena.alloc_in_place(|uninit| {
//         //     unsafe { std::ptr::copy(&obj, uninit.as_mut_ptr(), 1); }
//         // });
//     });
//     arena.stats();
// }

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};

pub fn criterion_benchmark(c: &mut Criterion) {
    // let mut arena = Arena::<MyStruct>::with_capacity(100000000);
    let shared_arena = SharedArena::<MyStruct>::with_capacity(10000000);
    let arena = Arena::<MyStruct>::with_capacity(10000000);
    let pool = Pool::<MyStruct>::with_capacity(10000000);

    // let my_struct = MyStruct::default();
    // let size = std::mem::size_of::<MyStruct>();

    // c.bench_with_input(BenchmarkId::new("input_default", 1), &my_struct, |b, s| {
    //     b.iter_with_large_drop(|| arena.alloc(black_box(*s)))
    // });

    // {
    //     let mut arena = Arena::<MyStruct>::with_capacity(10000000);

    //     c.bench_function("arena_with_drop", |b| {
    //         b.iter(|| arena.alloc(black_box(MyStruct::default())))
    //     });
    // }

    // let now = std::time::Instant::now();
    // for _ in 0..100_000 {
    //     Box::new(black_box(MyStruct::default()));
    //     // println!("STAT: {:?}", arena.stats());
    // }
    // println!("TIME2 {:?}", now.elapsed());

    // let now = std::time::Instant::now();
    // for _ in 0..100_000 {
    //     arena.alloc(black_box(MyStruct::default()));
    //     // println!("STAT: {:?}", arena.stats());
    // }
    // println!("TIME {:?}", now.elapsed());

    // c.bench_function("arena_1000", |b| {

    // });

    // let plot_config = PlotConfiguration::default()
    //     .summary_scale(AxisScale::Linear);

    // group.plot_config(plot_config);

    // // println!("STAT: {:?}", arena.stats());

    // group.bench_function("arena", |b| {
    //     //b.iter(|| arena.alloc(black_box(MyStruct::default())))
    //     b.iter_with_large_drop(|| arena.alloc(black_box(MyStruct::default())))
    // });

    // group.bench_function("new_arena", |b| {
    //     //b.iter(|| arena.alloc(black_box(MyStruct::default())))
    //     b.iter_with_large_drop(|| new_arena.alloc(black_box(MyStruct::default())))
    // });


    // return;

    let mut group = c.benchmark_group("SingleAlloc");

    // group.bench_function("arena_arc", |b| {
    //     b.iter_with_large_drop(|| arena.alloc_arc(black_box(MyStruct::default())))
    // });

    group.bench_function("SharedArena", |b| {
        b.iter_with_large_drop(|| shared_arena.alloc(black_box(MyStruct::default())))
    });

    group.bench_function("Arena", |b| {
        b.iter_with_large_drop(|| arena.alloc(black_box(MyStruct::default())))
    });

    group.bench_function("Pool", |b| {
        b.iter_with_large_drop(|| pool.alloc(black_box(MyStruct::default())))
    });

    group.bench_function("Box (System Allocator)", |b| {
        b.iter_with_large_drop(|| Box::new(black_box(MyStruct::default())))
    });

    // return;

    group.finish();

    let mut group = c.benchmark_group("Benchmark");
    for i in (1..=100_001).step_by(10000) {

        let i = (i - 1).max(1);

        // let mut vec = Vec::with_capacity(10_000_000);

        group.bench_with_input(BenchmarkId::new("Box (System Allocator)", i), &i, move |b, n| {
            let n = *n;

            b.iter_custom(move |iters| {
                let mut duration = Duration::new(0, 0);

                for _ in 0..iters {
                    let mut vec = Vec::with_capacity(n);

                    let start = Instant::now();
                    for _ in 0..n {
                        let res = Box::new(black_box(MyStruct::default()));
                        vec.push(black_box(res));
                    }
                    duration += start.elapsed()
                }

                duration
            });
        });

        group.bench_with_input(BenchmarkId::new("Arena", i), &i, move |b, n| {
            let n = *n;

            b.iter_custom(move |iters| {
                let mut duration = Duration::new(0, 0);

                for _ in 0..iters {
                    //println!("NEW {}", n);

                    let arena = Arena::<MyStruct>::with_capacity(n);
                    let mut vec = Vec::with_capacity(n);

                    let start = Instant::now();
                    for _ in 0..n {
                        let res = arena.alloc(black_box(MyStruct::default()));
                        vec.push(res);
                    }
                    duration += start.elapsed();
                    // arena.clean();
                }

                duration
            });
        });

        use std::time::{Instant, Duration};

        group.bench_with_input(BenchmarkId::new("Pool", i), &i, move |b, n| {
            let n = *n;

            b.iter_custom(move |iters| {
                let mut duration = Duration::new(0, 0);

                for _ in 0..iters {
                    let arena = Pool::<MyStruct>::with_capacity(n);
                    let mut vec = Vec::with_capacity(n);

                    let start = Instant::now();
                    for _ in 0..n {
                        let res = arena.alloc(black_box(MyStruct::default()));
                        vec.push(res);
                    }
                    duration += start.elapsed();
                }

                duration
            });
        });

        group.bench_with_input(BenchmarkId::new("SharedArena", i), &i, move |b, n| {
            let n = *n;

            b.iter_custom(move |iters| {
                let mut duration = Duration::new(0, 0);

                for _ in 0..iters {
                    let arena = SharedArena::<MyStruct>::with_capacity(n);
                    let mut vec = Vec::with_capacity(n);

                    let start = Instant::now();
                    for _ in 0..n {
                        let res = arena.alloc(black_box(MyStruct::default()));
                        vec.push(res);
                    }
                    duration += start.elapsed();
                }

                duration
            });
        });

        // group.bench_with_input(BenchmarkId::new("Iterative", i), i,
        //     |b, i| b.iter(|| fibonacci_fast(*i)));
    }
    group.finish();

}

criterion_group!{
    name = benches;
    config = Criterion::default()
        .with_plots()
        .warm_up_time(std::time::Duration::from_millis(100))
        // .measurement_time(std::time::Duration::from_secs(10))
        .sample_size(50);
    targets = criterion_benchmark
}

// criterion_group!(benches, criterion_benchmark);

criterion_main!(benches);

// #[bench]
// fn arena(b: &mut Bencher) {
//     println!("HELLO", );
//     let mut arena = Arena::<MyStruct>::with_capacity(220000000);
//     let obj = MyStruct::default();
//     // let mut vec = Vec::with_capacity(100000000);
//     println!("HELLO2", );

//     b.iter(|| {
//         //ManuallyDrop::new(arena.alloc(obj.clone()));
//         ManuallyDrop::new(arena.alloc(MyStruct::default()));
//         // ManuallyDrop::new(arena.alloc_in_place(|uninit| {
//         //     unsafe { std::ptr::copy(&obj, uninit.as_mut_ptr(), 1); }
//         // }))
//     });
//     arena.stats();

//     println!("NPAGES {:?}", arena.npages());
// }

// #[bench]
// fn arena_arc(b: &mut Bencher) {
//     println!("HELLO", );
//     let mut arena = Arena::<MyStruct>::with_capacity(100000000);
//     let obj = MyStruct::default();
//     // let mut vec = Vec::with_capacity(100000000);
//     println!("HELLO2", );

//     b.iter(|| {
//         //ManuallyDrop::new(arena.alloc(obj.clone()));
//         ManuallyDrop::new(arena.alloc_arc(MyStruct::default()));
//         // ManuallyDrop::new(arena.alloc_in_place(|uninit| {
//         //     unsafe { std::ptr::copy(&obj, uninit.as_mut_ptr(), 1); }
//         // }))
//     });
//     arena.stats();
// }

// #[bench]
// fn arena_with_drop(b: &mut Bencher) {
//     println!("HELLO", );
//     let mut arena = Arena::<MyStruct>::with_capacity(100000000);
//     let obj = MyStruct::default();
//     // let mut vec = Vec::with_capacity(100000000);
//     println!("HELLO2 {:?}", std::mem::size_of::<MyStruct>());

//     let mut i = 0;

//     b.iter(|| {
//         println!("I={}", i);
//         i += 1;
//         //ManuallyDrop::new(arena.alloc(obj.clone()));
//         let res = arena.alloc(MyStruct::default());
//         test::black_box(res)

//         // ManuallyDrop::new(arena.alloc_in_place(|uninit| {
//         //     unsafe { std::ptr::copy(&obj, uninit.as_mut_ptr(), 1); }
//         // }))
//     });
//     arena.stats();
// }

// #[bench]
// fn normal_alloc_drop(b: &mut Bencher) {
//     let obj = MyStruct::default();

//     //let mut vec = Vec::with_capacity(100000000);

//     use std::sync::Arc;

//     b.iter(|| {
//         let res = Arc::new(MyStruct::default());
//         test::black_box(res)
//     });

//     // println!("LEN {:?}", vec.len());
//     //assert!(vec.len() == 1000, format!("VEC_LEN {:?}", vec.len()));
// }

// #[bench]
// fn arena_2(b: &mut Bencher) {
//     let mut arena = Arena::with_capacity(10000000);
//     let obj = MyStruct::default();
//     let mut vec = Vec::with_capacity(10000000);

//     b.iter(|| {
//         for _ in 0..131_072 {
//             let value = arena.alloc(10);
//             vec.push(value);
//         }
//         vec.clear();
//     });
// }

// #[bench]
// fn normal_2(b: &mut Bencher) {
//     // let mut arena = Arena::with_capacity(10000000);
//     // let obj = MyStruct::default();
//     let mut vec = Vec::with_capacity(10000000);

//     b.iter(|| {
//         for _ in 0..131_072 {
//             let value = Box::new(10);
//             vec.push(value);
//         }
//         vec.clear();
//     });
// }

// #[bench]
// fn test_and(b: &mut Bencher) {
//     let mut var = 1;
//     b.iter(|| {
//         var *= 2;
//         let mask = var - 1;
//         var & mask
//     });
// }

// #[bench]
// fn test_modulo(b: &mut Bencher) {
//     let mut var = 1;
//     b.iter(|| {
//         var *= 2;
//         let mask = var - 1;
//         var % mask
//     });
// }


// #[bench]
// fn mem_access(b: &mut Bencher) {

//     use std::sync::atomic::AtomicUsize;

//     let mut array = Vec::with_capacity(64 * 1024 * 1024);
//     array.resize_with(64 * 1024 * 1024, || {
//         AtomicUsize::new(0)
//     });

//     b.iter(|| {

//         // ManuallyDrop::new(arena.alloc(obj.clone()));
// //        ManuallyDrop::new(arena.alloc(MyStruct::default()));
//         // ManuallyDrop::new(arena.alloc_in_place(|uninit| {
//         //     unsafe { std::ptr::copy(&obj, uninit.as_mut_ptr(), 1); }
//         // }))
//     });
//     arena.stats();
// }

// http://igoro.com/archive/gallery-of-processor-cache-effects/
// https://software.intel.com/en-us/articles/avoiding-and-identifying-false-sharing-among-threads
// https://fgiesen.wordpress.com/2014/08/18/atomics-and-contention/

// #[bench]
// fn normal_alloc(b: &mut Bencher) {
//     let obj = MyStruct::default();

//     //let mut vec = Vec::with_capacity(100000000);

//     b.iter(|| {
//         ManuallyDrop::new(Box::new(MyStruct::default()))
//     });

//     // println!("LEN {:?}", vec.len());
//     //assert!(vec.len() == 1000, format!("VEC_LEN {:?}", vec.len()));
// }

// #[bench]
// fn normal_vec(b: &mut Bencher) {
//     let mut vec = Vec::with_capacity(1000);
//     let obj = MyStruct::default();

//     b.iter(|| {
//         // vec.clear();
//         // for _ in 0..1000 {
//         unsafe {
//             let mut a = std::mem::uninitialized();
//             std::ptr::copy(&obj, &mut a as *mut MyStruct, 1);
//             vec.push(a);
//         }
//         // }
//     });

//     println!("LEN {:?}", vec.len());
//     //assert!(vec.len() == 1000, format!("VEC_LEN {:?}", vec.len()));
// }
