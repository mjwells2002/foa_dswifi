use defmt::info;
use embassy_sync::blocking_mutex::NoopMutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};
use embassy_sync::channel::{Channel, Sender};

#[macro_export] macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        #[allow(static_mut_refs)]
        static STATIC_DRAM : static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_DRAM.uninit().write(($val));
        x
    }};
}
#[macro_export] macro_rules! mk_static_dram2 {
    ($t:ty,$val:expr) => {{
        unsafe {
            #[allow(static_mut_refs)]
            #[link_section =".dram2_uninit"]
            static mut STATIC_DRAM2 : MaybeUninit<$t> = MaybeUninit::uninit();
            #[deny(unused_attributes)]
            #[allow(static_mut_refs)]
            let x = STATIC_DRAM2.write(($val));
            x
        }
    }};
}

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

//helper for files included in the binary
pub fn get_file(name: &str) -> Option<&'static [u8]> {
    for (n, data) in FILES {
        if *n == name {
            return Some(*data);
        }
    }
    None
}

pub struct TaskMailbox<Mutex ,CMD, RESP, const N: usize> where Mutex: RawMutex {
    cmd_channel: Channel<Mutex, (CMD,usize), N>,
    pool: MailboxReplyPool<Mutex, RESP, N>,
}

pub struct MailboxReplyPool<Mutex, RESP, const N: usize> where Mutex: RawMutex {
    pool: [Channel<Mutex, RESP, 1>; N],
    freelist: Channel<Mutex, usize, N>,
}

impl<Mutex, CMD, RESP, const N: usize>
TaskMailbox<Mutex, CMD, RESP, N> where Mutex: RawMutex
{
    pub const fn new() -> Self {
        Self {
            cmd_channel: Channel::new(),
            pool: MailboxReplyPool::new(),
        }
    }

    pub fn init(&'static self) {
        self.pool.init();
    }

    pub async fn request(&'static self, cmd: CMD) -> RESP {
        let slot = self.pool.acquire().await;
        self.cmd_channel.send((cmd,slot.index)).await;
        slot.receive().await
    }

    pub async fn receive(&'static self) -> (CMD, Sender<'_, Mutex, RESP, 1>) {
        let (cmd,slot_idx) = self.cmd_channel.receive().await;
        (cmd,self.pool.pool[slot_idx].sender())
    }
}
impl<Mutex, RESP, const N: usize> MailboxReplyPool<Mutex, RESP, N> where Mutex: RawMutex {
    pub const fn new() -> Self {
        Self {
            pool: [const { Channel::new() }; N],
            freelist: Channel::new(),
        }
    }

    pub fn init(&'static self) {
        for i in 0..N {
            self.freelist.try_send(i).expect("MailboxReplyPool init failed");
        }
    }

    pub async fn acquire(&'static self) -> MailboxReplySlot<Mutex, RESP, N> {
        let index = self.freelist.receive().await;
        MailboxReplySlot { pool: self, index }
    }
}


pub struct MailboxReplySlot<'a, Mutex, RESP, const N: usize> where Mutex: RawMutex {
    pool: &'a MailboxReplyPool<Mutex, RESP, N>,
    index: usize,
}

impl<'a, Mutex, RESP, const N: usize> MailboxReplySlot<'a, Mutex, RESP, N> where Mutex: RawMutex {
    pub fn sender(&self) -> Sender<'a, Mutex, RESP, 1> {
        self.pool.pool[self.index].sender()
    }

    pub async fn receive(&self) -> RESP {
        self.pool.pool[self.index].receive().await
    }
}

impl<'a, Mutex, RESP, const N: usize> Drop for MailboxReplySlot<'a, Mutex, RESP, N> where Mutex: RawMutex {
    fn drop(&mut self) {
        let _ = self.pool.pool[self.index].try_receive();
        self.pool.freelist.try_send(self.index).unwrap();
    }
}