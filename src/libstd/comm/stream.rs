// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/// Stream channels
///
/// This is the flavor of channels which are optimized for one sender and one
/// receiver. The sender will be upgraded to a shared channel if the channel is
/// cloned.
///
/// High level implementation details can be found in the comment of the parent
/// module.

use comm::Port;
use int;
use iter::Iterator;
use kinds::Send;
use ops::Drop;
use option::{Option, Some, None};
use result::{Ok, Err, Result};
use rt::local::Local;
use rt::task::{Task, BlockedTask};
use spsc = sync::spsc_queue;
use sync::atomics;
use vec::OwnedVector;

static DISCONNECTED: int = int::min_value;

pub struct Packet<T> {
    queue: spsc::Queue<Message<T>>, // internal queue for all message

    cnt: atomics::AtomicInt, // How many items are on this channel
    steals: int, // How many times has a port received without blocking?
    to_wake: Option<BlockedTask>, // Task to wake up

    go_home: atomics::AtomicBool, // flag if the channel has been destroyed.
}

pub enum Failure<T> {
    Empty,
    Disconnected,
    Upgraded(Port<T>),
}

// Any message could contain an "upgrade request" to a new shared port, so the
// internal queue it's a queue of T, but rather Message<T>
enum Message<T> {
    Data(T),
    GoUp(Port<T>),
}

impl<T: Send> Packet<T> {
    pub fn new() -> Packet<T> {
        Packet {
            queue: spsc::Queue::new(128),

            cnt: atomics::AtomicInt::new(0),
            steals: 0,
            to_wake: None,

            go_home: atomics::AtomicBool::new(false),
        }
    }


    pub fn send(&mut self, t: T) -> bool { self.do_send(Data(t)) }
    pub fn upgrade(&mut self, up: Port<T>) -> bool { self.do_send(GoUp(up)) }

    fn do_send(&mut self, t: Message<T>) -> bool {
        // Use an acquire/release ordering to maintain the same position with
        // respect to the atomic loads below
        if self.go_home.load(atomics::AcqRel) { return false }

        self.queue.push(t);
        match self.cnt.fetch_add(1, atomics::SeqCst) {
            // As described in the mod's doc comment, -1 == wakeup
            -1 => { self.wakeup(); true }
            // As as described before, SPSC queues must be >= -2
            -2 => true,

            // Be sure to preserve the disconnected state, and the return value
            // in this case is going to be whether our data was received or not.
            // This manifests itself on whether we have an empty queue or not.
            //
            // Primarily, are required to drain the queue here because the port
            // will never remove this data. We can only have at most one item to
            // drain (the port drains the rest).
            DISCONNECTED => {
                self.cnt.store(DISCONNECTED, atomics::SeqCst);
                let first = self.queue.pop();
                let second = self.queue.pop();
                assert!(second.is_none());

                match first {
                    Some(..) => false, // we failed to send the data
                    None => true,      // we successfully sent data
                }
            }

            // Otherwise we just sent some data on a non-waiting queue, so just
            // make sure the world is sane and carry on!
            n => { assert!(n >= 0); true }
        }
    }

    pub fn recv(&mut self) -> Result<T, Failure<T>> {
        // optimistic preflight check (scheduling is expensive)
        match self.try_recv() {
            Err(Empty) => {}
            data => return data,
        }

        // Welp, our channel has no data. Deschedule the current task and
        // initiate the blocking protocol. Note that this is the location at
        // which we take the number of steals into account (because we're
        // guaranteed to block).
        let task: ~Task = Local::take();
        task.deschedule(1, |task| {
            assert!(self.to_wake.is_none());
            self.to_wake = Some(task);
            let steals = self.steals;
            self.steals = 0;

            match self.cnt.fetch_sub(1 + steals, atomics::SeqCst) {
                // If the other side went away, we wake ourselves back up to go
                // back into try_recv
                DISCONNECTED => {
                    self.cnt.store(DISCONNECTED, atomics::SeqCst);
                    Err(self.to_wake.take_unwrap())
                }

                // If we factor in our steals and notice that the channel has no
                // data, we successfully sleep
                n if n - steals <= 0 => Ok(()),

                // Someone snuck in and sent data, cancelling our sleep. So sad.
                _ => Err(self.to_wake.take_unwrap()),
            }
        });

        match self.try_recv() {
            // Messages which actually popped from the queue shouldn't count as
            // a steal, so offset the decrement here (we already have our
            // "steal" factored into the channel count above).
            data @ Ok(..) |
            data @ Err(Upgraded(..)) => {
                self.steals -= 1;
                data
            }

            data => data,
        }
    }

    pub fn try_recv(&mut self) -> Result<T, Failure<T>> {
        match self.queue.pop() {
            // If we stole some data, record to that effect (this will be
            // factored into cnt later on)
            Some(data) => {
                self.steals += 1;
                match data {
                    Data(t) => Ok(t),
                    GoUp(up) => Err(Upgraded(up)),
                }
            }

            None => {
                match self.cnt.load(atomics::SeqCst) {
                    n if n != DISCONNECTED => Err(Empty),

                    // This is a little bit of a tricky case. We failed to pop
                    // data above, and then we have viewed that the channel is
                    // disconnected. In this window more data could have been
                    // sent on the channel. It doesn't really make sense to
                    // return that the channel is disconnected when there's
                    // actually data on it, so be extra sure there's no data by
                    // popping one more time.
                    //
                    // We can ignore steals because the other end is
                    // disconnected and we'll never need to really factor in our
                    // steals again.
                    _ => {
                        match self.queue.pop() {
                            Some(Data(t)) => Ok(t),
                            Some(GoUp(up)) => Err(Upgraded(up)),
                            None => Err(Disconnected),
                        }
                    }
                }
            }
        }
    }

    pub fn drop_chan(&mut self) {
        // Dropping a channel is pretty simple, we just flag it as disconnected
        // and then wakeup a blocker if there is one.
        match self.cnt.swap(DISCONNECTED, atomics::SeqCst) {
            -1 => { self.wakeup(); }
            DISCONNECTED => {}
            n => { assert!(n >= 0); }
        }
    }

    pub fn drop_port(&mut self) {
        // Dropping a port seems like a fairly trivial thing. In theory all we
        // need to do is flag that we're disconnected and then everything else
        // can take over (we don't have anyone to wake up).
        //
        // The catch for Ports is that we want to drop the entire contents of
        // the queue. There are multiple reasons for having this property, the
        // largest of which is that if another chan is waiting in this channel
        // (but not received yet), then waiting on that port will cause a
        // deadlock.
        //
        // So if we accept that we must now destroy the entire contents of the
        // queue, this code may make a bit more sense. The tricky part is that
        // we can't let any in-flight sends go un-dropped, we have to make sure
        // *everything* is dropped and nothing new will come onto the channel.

        // The first thing we do is set a flag saying that we're done for. All
        // sends are gated on this flag, so we're immediately guaranteed that
        // there are a bounded number of active sends that we'll have to deal
        // with.
        self.go_home.store(true, atomics::SeqCst);

        // Now that we're guaranteed to deal with a bounded number of senders,
        // we need to drain the queue. This draining process happens atomically
        // with respect to the "count" of the channel. If the count is nonzero
        // (with steals taken into account), then there must be data on the
        // channel. In this case we drain everything and then try again. We will
        // continue to fail while active senders send data while we're dropping
        // data, but eventually we're guaranteed to break out of this loop
        // (because there is a bounded number of senders).
        let mut steals = self.steals;
        while {
            let cnt = self.cnt.compare_and_swap(
                            steals, DISCONNECTED, atomics::SeqCst);
            cnt != DISCONNECTED && cnt != steals
        } {
            loop {
                match self.queue.pop() {
                    Some(..) => { steals += 1; }
                    None => break
                }
            }
        }

        // At this point in time, we have gated all future senders from sending,
        // and we have flagged the channel as being disconnected. The senders
        // still have some responsibility, however, because some sends may not
        // complete until after we flag the disconnection. There are more
        // details in the sending methods that see DISCONNECTED
    }

    // This function must have had at least an acquire fence before it to be
    // properly called.
    fn wakeup(&mut self) {
        self.to_wake.take_unwrap().wake().map(|t| t.reawaken(true));
    }
}

#[unsafe_destructor]
impl<T: Send> Drop for Packet<T> {
    fn drop(&mut self) {
        unsafe {
            // Note that this load is not only an assert for correctness about
            // disconnection, but also a proper fence before the read of
            // `to_wake`, so this assert cannot be removed with also removing
            // the `to_wake` assert.
            assert_eq!(self.cnt.load(atomics::SeqCst), DISCONNECTED);
            assert!(self.to_wake.is_none());
        }
    }
}
