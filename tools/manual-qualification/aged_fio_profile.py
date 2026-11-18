MIB = 1024 ** 2
LOGICAL = 4096 * MIB
# At most 20% more raw capacity than logical size, rounded down to whole MiB.
FIO_BACKING = 4915 * MIB


def fio_args(job, fd, seed, prefix):
    if job not in ('fill', 'uniform', 'zipf') or fd < 3 or not 0 <= seed < 2 ** 31:
        raise ValueError('bad fio job, inherited device FD or seed')
    args = ['fio', '--name=' + job, '--filename=/proc/self/fd/' + str(fd),
            '--allow_file_create=0', '--size=' + str(LOGICAL), '--bs=4k',
            '--direct=1', '--ioengine=io_uring', '--iodepth=32', '--thread=1',
            '--numjobs=1', '--end_fsync=1', '--output-format=json+',
            '--log_avg_msec=1000', '--log_hist_msec=1000',
            '--write_bw_log=' + str(prefix), '--write_iops_log=' + str(prefix),
            '--write_hist_log=' + str(prefix), '--output=' + str(prefix) + '.json']
    if job == 'fill':
        return args + ['--rw=write', '--io_size=' + str(LOGICAL)]
    args += ['--rw=randwrite', '--fsync=256', '--io_size=' + str(FIO_BACKING),
             '--randseed=' + str(seed), '--norandommap=1']
    if job == 'zipf':
        args += ['--random_distribution=zipf:1.2']
    return args
