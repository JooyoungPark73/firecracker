mod uffd_utils;

use std::fs::File;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uffd_utils::{MemPageState, Runtime, UffdHandler};

fn main() {
    let mut args = std::env::args();
    let uffd_sock_path = args.nth(1).expect("No socket path given");
    let mem_file_path = args.next().expect("No memory file given");

    let file = File::open(mem_file_path).expect("Cannot open memfile");

    // Get Uffd from UDS. We'll use the uffd to handle PFs for Firecracker.
    let listener = UnixListener::bind(uffd_sock_path).expect("Cannot bind to socket path");
    let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

    let mut runtime = Runtime::new(stream, file);

    // Introduce a counter for page faults
    let page_fault_counter = Arc::new(AtomicUsize::new(0));
    let page_fault_counter_clone = Arc::clone(&page_fault_counter);

    runtime.run(move |uffd_handler: &mut UffdHandler| {
        // Read an event from the userfaultfd.
        let event = uffd_handler
            .read_event()
            .expect("Failed to read uffd_msg")
            .expect("uffd_msg not ready");

        // We expect to receive either a Page Fault or Removed
        // event (if the balloon device is enabled).
        match event {
            userfaultfd::Event::Pagefault { addr, .. } => {
                uffd_handler.serve_pf(addr.cast(), uffd_handler.page_size);
                // Increment the page fault counter
                page_fault_counter_clone.fetch_add(1, Ordering::SeqCst);
            }
            userfaultfd::Event::Remove { start, end } => uffd_handler.update_mem_state_mappings(
                start as u64,
                end as u64,
                MemPageState::Removed,
            ),
            _ => panic!("Unexpected event on userfaultfd"),
        }
        
        // Print the current count of pages served
        println!("Pages served: {}", page_fault_counter_clone.load(Ordering::SeqCst));
    });
}
