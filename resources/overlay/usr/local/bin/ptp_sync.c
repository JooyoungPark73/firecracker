// ptp_sync.c
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>      // Added for errno
#include <sys/types.h>

#ifndef CLOCKFD
#define CLOCKFD 3
#endif
#define FD_TO_CLOCKID(fd)   ((~(clockid_t) (fd) << 3) | CLOCKFD)

// Helper to calculate time difference
double get_time_diff(struct timespec start, struct timespec end) {
    double s = (double)end.tv_sec - (double)start.tv_sec;
    double ns = (double)end.tv_nsec - (double)start.tv_nsec;
    return s + (ns / 1000000000.0);
}

int main() {
    int ret = 0;
    int err_save = 0; // To capture errno
    
    int fd_ptp = open("/dev/ptp0", O_RDONLY);
    if (fd_ptp < 0) return 1;

    // Use /dev/kmsg for kernel-level logging
    int fd_kmsg = open("/dev/kmsg", O_WRONLY);
    
    clockid_t ptp_clk = FD_TO_CLOCKID(fd_ptp);
    struct timespec ts_ptp, ts_sys_before;

    clock_gettime(CLOCK_REALTIME, &ts_sys_before);
    
    if (clock_gettime(ptp_clk, &ts_ptp) == -1) {
        close(fd_ptp);
        if (fd_kmsg >= 0) close(fd_kmsg);
        return 1;
    }

    // --- ATTEMPT SYNC ---
    if (clock_settime(CLOCK_REALTIME, &ts_ptp) == -1) {
        ret = 1;
        err_save = errno; // Capture the specific error
    }

    // --- LOGGING ---
    if (fd_kmsg >= 0) {
        char log_msg[256];
        if (ret == 0) {
            double diff = get_time_diff(ts_sys_before, ts_ptp);
            snprintf(log_msg, sizeof(log_msg), 
                "<6>vmgenid_sync: Time synced. Adjustment: %+.9f sec\n", diff);
        } else {
            // Log the specific error string (e.g., "Operation not permitted")
            snprintf(log_msg, sizeof(log_msg), 
                "<3>vmgenid_sync: FAILED to set system time. Error: %s\n", strerror(err_save));
        }
        write(fd_kmsg, log_msg, strlen(log_msg));
        close(fd_kmsg);
    }

    close(fd_ptp);
    return ret;
}