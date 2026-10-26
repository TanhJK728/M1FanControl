package M1FanDeploy;
use strict;
use warnings;
use Fcntl qw(:DEFAULT :flock :mode F_GETFL F_SETFL O_NONBLOCK F_SETFD FD_CLOEXEC);
use Digest::SHA;
use JSON::PP;
use IO::Select;
use IO::Handle;
use Socket qw(AF_UNIX SOCK_STREAM sockaddr_un);
use POSIX qw(WNOHANG);
use Time::HiRes qw(time sleep clock_gettime CLOCK_MONOTONIC);
use Errno qw(EAGAIN EWOULDBLOCK EINTR);

our $LABEL = 'local.m1fan.control';
our @PAYLOAD = qw(m1fan manage.pl ServiceInstall.pm enable.sh disable.sh status.sh activate.sh deactivate.sh uninstall.sh);
our $VERSION = '0.5.0';
our $R5_PLIST_SHA = '01bcf45afaa7387d325cc661b0d7ac399a2d4ffba901121c26bfa1f0c53b8960';

sub plist {
    return <<'PLIST';
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>local.m1fan.control</string>
    <key>ProgramArguments</key>
    <array><string>/Library/Application Support/M1FanControl/m1fan</string><string>serve</string></array>
    <key>UserName</key><string>root</string>
    <key>GroupName</key><string>wheel</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>ThrottleInterval</key><integer>60</integer>
    <key>Disabled</key><true/>
    <key>ExitTimeOut</key><integer>30</integer>
    <key>Umask</key><integer>63</integer>
</dict>
</plist>
PLIST
}

sub production {
    my ($source) = @_;
    return {
        uid => 0, source => $source,
        parents => ['/', '/Library', '/Library/Application Support', '/Library/LaunchDaemons'],
        app => '/Library/Application Support/M1FanControl',
        plist => "/Library/LaunchDaemons/$LABEL.plist",
        lock => '/private/var/run/m1fan-control.lock',
        management_lock => '/private/var/run/m1fan-management.lock',
        history => '/Library/Application Support/M1FanControl-history',
        runtime => '/private/var/run/m1fan',
        launchctl => '/bin/launchctl',
    };
}

sub checked_directory {
    my ($path, $uid) = @_;
    my @s = lstat $path;
    die "missing directory $path\n" unless @s;
    die "unsafe directory $path (requires real directory, owner UID $uid, no group/world writes)\n"
        unless S_ISDIR($s[2]) && $s[4] == $uid && ($s[2] & 0022) == 0;
}

sub checked_open {
    my ($path, $uid, $flags, $mode) = @_;
    sysopen(my $fh, $path, $flags | O_NOFOLLOW, $mode // 0600)
        or die "open $path: $!\n";
    my @s = stat $fh;
    die "unsafe file $path (ownership/type/link count/permissions)\n"
        unless @s && S_ISREG($s[2]) && $s[3] == 1 && $s[4] == $uid && ($s[2] & 0022) == 0;
    fcntl($fh, F_SETFD, FD_CLOEXEC) or die "protect descriptor $path: $!\n";
    return $fh;
}

sub digest_handle {
    my ($fh) = @_;
    seek($fh, 0, 0) or die "seek: $!\n";
    return Digest::SHA->new(256)->addfile($fh)->hexdigest;
}

sub digest_file {
    my ($path, $uid) = @_;
    my $fh = checked_open($path, $uid, O_RDONLY);
    return digest_handle($fh);
}

sub check_parents {
    my ($cfg) = @_;
    checked_directory($_, $cfg->{uid}) for @{$cfg->{parents}};
}

sub try_lock {
    my ($cfg) = @_;
    my $fh = checked_open($cfg->{lock}, $cfg->{uid}, O_RDWR | O_CREAT, 0600);
    return $fh if flock($fh, LOCK_EX | LOCK_NB);
    return undef if $! == EWOULDBLOCK || $! == EAGAIN;
    die "lock $cfg->{lock}: $!\n";
}

# The only timed-out processes terminated here are this installer's direct
# launchctl/IPC client children. The controller PID is never signalled or killed.
sub command {
    my ($cfg, $timeout, @argv) = @_;
    if ($cfg->{runner}) {
        my @result = $cfg->{runner}->($timeout, @argv);
        log_event($cfg, "command @argv exit=$result[0]\n$result[1]");
        return @result;
    }
    die "absolute command required\n" unless $argv[0] =~ m{^/};
    pipe(my $reader, my $writer) or die "pipe: $!\n";
    my $pid = fork();
    die "fork: $!\n" unless defined $pid;
    if ($pid == 0) {
        close $reader;
        open STDIN, '<', '/dev/null' or POSIX::_exit(126);
        open STDOUT, '>&', $writer or POSIX::_exit(126);
        open STDERR, '>&', $writer or POSIX::_exit(126);
        close $writer;
        %ENV = (PATH => '/usr/bin:/bin:/usr/sbin:/sbin', LC_ALL => 'C');
        exec {$argv[0]} @argv or POSIX::_exit(127);
    }
    close $writer;
    my $flags = fcntl($reader, F_GETFL, 0);
    fcntl($reader, F_SETFL, $flags | O_NONBLOCK) or die "nonblocking command pipe: $!\n";
    my $select = IO::Select->new($reader);
    my $deadline = time + $timeout;
    my ($output, $status) = ('', undef);
    while (1) {
        if ($select->can_read(0.05)) {
            my $n = sysread($reader, my $buffer, 4096);
            $output .= $buffer if defined($n) && $n;
            last if length($output) > 65536;
        }
        if (waitpid($pid, WNOHANG) == $pid) { $status = $?; last; }
        last if time >= $deadline;
    }
    if (!defined $status) {
        kill 'TERM', $pid;
        my $hardware_writer = @argv > 1 && $argv[1] eq 'auto';
        my $end = time + ($hardware_writer ? 5 : 1);
        while (time < $end) {
            if (waitpid($pid, WNOHANG) == $pid) { $status = $?; last; }
            sleep 0.02;
        }
        if (!defined($status) && !$hardware_writer) { kill 'KILL', $pid; waitpid($pid, 0); }
        close $reader;
        die "command exceeded ${timeout}s/output limit: @argv; inspect service state before retrying\n";
    }
    while (1) {
        my $n = sysread($reader, my $buffer, 4096);
        last unless defined($n) && $n;
        $output .= $buffer;
        die "command output exceeds 64 KiB\n" if length($output) > 65536;
    }
    close $reader;
    my $code = ($status & 127) ? 128 + ($status & 127) : $status >> 8;
    log_event($cfg, "command @argv exit=$code\n$output");
    return ($code, $output);
}

sub require_command {
    my ($cfg, $timeout, @argv) = @_;
    my ($code, $output) = command($cfg, $timeout, @argv);
    die "command failed ($code): @argv\n$output\n" if $code;
    return $output;
}

sub loaded {
    my ($cfg, $timeout) = @_;
    my ($code, $output) = command($cfg, $timeout // 3, $cfg->{launchctl}, 'print', "system/$LABEL");
    return $output if $code == 0;
    return undef if $output =~ /Could not find service "\Q$LABEL\E" in domain for system/;
    die "cannot determine whether service is loaded ($code): $output\n";
}

sub source_bytes {
    my ($path) = @_;
    sysopen(my $fh, $path, O_RDONLY | O_NOFOLLOW) or die "open source $path: $!\n";
    my @s = stat $fh;
    die "source must be a regular file: $path\n" unless @s && S_ISREG($s[2]);
    my $bytes = '';
    while (1) {
        my $n = sysread($fh, my $buffer, 65536);
        die "read source $path: $!\n" unless defined $n;
        last unless $n;
        $bytes .= $buffer;
        die "source exceeds 32 MiB: $path\n" if length($bytes) > 32 * 1024 * 1024;
    }
    return $bytes;
}

my $sequence = 0;
sub create_file {
    my ($path, $bytes, $mode, $uid) = @_;
    die "refusing existing installation file $path\n" if lstat $path;
    my $temporary = "$path.new.$$-" . ++$sequence;
    sysopen(my $fh, $temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0600)
        or die "create $temporary: $!\n";
    my $ok = eval {
        binmode $fh;
        print {$fh} $bytes or die "write $temporary: $!\n";
        $fh->flush or die "flush $temporary: $!\n";
        $fh->sync or die "sync $temporary: $!\n";
        close $fh or die "close $temporary: $!\n";
        chown($uid, $uid == 0 ? 0 : -1, $temporary) == 1 or die "chown $temporary: $!\n";
        chmod($mode, $temporary) == 1 or die "chmod $temporary: $!\n";
        rename($temporary, $path) or die "publish $path: $!\n";
        1;
    };
    if (!$ok) { my $error = $@; close $fh; unlink $temporary; die $error; }
}

sub receipt {
    my ($cfg) = @_;
    checked_directory($cfg->{app}, $cfg->{uid});
    my $fh = checked_open("$cfg->{app}/installation.json", $cfg->{uid}, O_RDONLY);
    local $/;
    my @metadata = stat $fh;
    die "oversized installation receipt\n" if $metadata[7] > 32768;
    my $text = <$fh>;
    my $receipt = JSON::PP->new->decode($text);
    if (ref($receipt) eq 'HASH' && ($receipt->{schema} // 0) == 1
        && ($receipt->{plist_sha256} // '') eq $R5_PLIST_SHA) {
        die "R5 installation detected. Preserve its receipt and evidence, then use its installed R5 deactivate.sh/uninstall.sh before installing R6. No migration or removal was performed.\n";
    }
    die "unknown installation receipt/version; preserve all files and inspect before migration\n"
        unless ref($receipt) eq 'HASH' && ($receipt->{schema} // 0) == 2
        && ($receipt->{version} // '') eq $VERSION && ref($receipt->{files}) eq 'HASH';
    my @names = sort keys %{$receipt->{files}};
    die "unexpected files in installation receipt\n" unless join(',', @names) eq join(',', sort @PAYLOAD);
    for my $name (@PAYLOAD) {
        my $actual = digest_file("$cfg->{app}/$name", $cfg->{uid});
        die "installed $name differs from its receipt; preserve and inspect before proceeding\n"
            unless $actual eq $receipt->{files}{$name};
    }
    die "installed plist differs from receipt\n" unless digest_file($cfg->{plist}, $cfg->{uid}) eq $receipt->{plist_sha256};
    die "unexpected installed launchd settings\n" unless $receipt->{plist_sha256} eq Digest::SHA::sha256_hex(plist());
    marker($cfg);
    return $receipt;
}

sub install {
    my ($cfg, $binary, $expected) = @_;
    die "usage: install.sh ABSOLUTE_BINARY_PATH EXPECTED_SHA256\n"
        unless defined($binary) && $binary =~ m{^/} && defined($expected) && $expected =~ /\A[0-9a-fA-F]{64}\z/;
    check_parents($cfg);
    die "service is already loaded; deactivate before installation\n" if defined loaded($cfg);
    my $lock = try_lock($cfg) or die "controller owns the control lock; complete m1fan stop or foreground Ctrl+C first\n";
    if (lstat("$cfg->{app}/installation.json")) {
        receipt($cfg);
        die "R6 installation already exists; preserve it and uninstall explicitly before replacement\n";
    }
    die "unknown installation exists; preserve all files before migration\n"
        if lstat($cfg->{plist}) || lstat("$cfg->{app}/m1fan");
    my $binary_bytes = source_bytes($binary);
    die "source SHA-256 does not match the supplied release hash; nothing installed\n"
        unless Digest::SHA::sha256_hex($binary_bytes) eq lc $expected;
    my %bytes = (m1fan => $binary_bytes);
    $bytes{$_} = source_bytes("$cfg->{source}/$_") for grep { $_ ne 'm1fan' } @PAYLOAD;
    if (!lstat $cfg->{app}) {
        mkdir($cfg->{app}, 0755) or die "mkdir $cfg->{app}: $!\n";
        chmod(0755, $cfg->{app}) == 1 or die "chmod $cfg->{app}: $!\n";
    }
    checked_directory($cfg->{app}, $cfg->{uid});
    opendir(my $directory, $cfg->{app}) or die "opendir $cfg->{app}: $!\n";
    my @contents = grep { $_ ne '.' && $_ ne '..' } readdir $directory;
    closedir $directory;
    die "application directory must be empty for a new installation\n" if @contents;
    # A previously enabled override can survive an uninstall. Disable explicitly
    # before publishing RunAtLoad so installation alone cannot start on reboot.
    require_command($cfg, 5, $cfg->{launchctl}, 'disable', "system/$LABEL");
    my @created;
    my $ok = eval {
        my %hashes;
        for my $name (@PAYLOAD) {
            my $mode = $name eq 'ServiceInstall.pm' ? 0644 : 0755;
            create_file("$cfg->{app}/$name", $bytes{$name}, $mode, $cfg->{uid});
            push @created, "$cfg->{app}/$name";
            $hashes{$name} = Digest::SHA::sha256_hex($bytes{$name});
        }
        create_file($cfg->{plist}, plist(), 0644, $cfg->{uid});
        push @created, $cfg->{plist};
        create_file("$cfg->{app}/service.enabled", "disabled\n", 0600, $cfg->{uid});
        push @created, "$cfg->{app}/service.enabled";
        my $receipt = JSON::PP->new->canonical->pretty->encode({schema => 2, version => $VERSION, files => \%hashes,
            plist_sha256 => Digest::SHA::sha256_hex(plist()), source_sha256 => lc $expected});
        create_file("$cfg->{app}/installation.json", $receipt, 0600, $cfg->{uid});
        push @created, "$cfg->{app}/installation.json";
        receipt($cfg);
        1;
    };
    if (!$ok) { my $error = $@; unlink $_ for reverse @created; die $error; }
    note($cfg, "Installed verified binary and root-owned launchd definition. Service remains disabled.\n");
    note($cfg, "Enable explicitly: sudo \"$cfg->{app}/enable.sh\"\n");
}

sub private_directory {
    my ($path, $uid) = @_;
    if (!lstat $path) {
        mkdir($path, 0700) or die "mkdir $path: $!\n";
        chmod(0700, $path) == 1 or die "chmod $path: $!\n";
    }
    checked_directory($path, $uid);
    die "private directory must have mode 0700: $path\n" unless ((lstat $path)[2] & 0777) == 0700;
}

sub management_lock {
    my ($cfg) = @_;
    my $fh = checked_open($cfg->{management_lock}, $cfg->{uid}, O_RDWR | O_CREAT, 0600);
    die "another deployment operation holds the management lock; retry after it completes\n"
        unless flock($fh, LOCK_EX | LOCK_NB);
    return $fh;
}

sub log_event {
    my ($cfg, $text) = @_;
    return unless $cfg->{transaction_fh};
    my $fh = $cfg->{transaction_fh};
    print {$fh} '[' . time . "] $text\n" or die "write deployment transaction: $!\n";
    $fh->flush or die "flush deployment transaction: $!\n";
}

sub note {
    my ($cfg, $text) = @_;
    print $text;
    log_event($cfg, $text);
}

sub marker {
    my ($cfg) = @_;
    my $path = "$cfg->{app}/service.enabled";
    my $fh = checked_open($path, $cfg->{uid}, O_RDONLY);
    my @s = stat $fh;
    die "service intent must have mode 0600 and bounded size\n" unless ($s[2] & 0777) == 0600 && $s[7] <= 16;
    local $/;
    my $text = <$fh>;
    return 1 if $text eq "enabled\n";
    return 0 if $text eq "disabled\n";
    die "invalid service.enabled intent marker\n";
}

sub set_marker {
    my ($cfg, $enabled) = @_;
    marker($cfg); # Refuse replacement of a symlink, corrupt or untrusted file.
    my $path = "$cfg->{app}/service.enabled";
    my $temporary = "$path.intent.$$-" . ++$sequence;
    create_file($temporary, $enabled ? "enabled\n" : "disabled\n", 0600, $cfg->{uid});
    if (!rename($temporary, $path)) { my $error = $!; unlink $temporary; die "publish service intent: $error\n"; }
    checked_directory($cfg->{app}, $cfg->{uid});
    # fsync the containing directory so disable intent survives a sudden restart.
    sysopen(my $dir, $cfg->{app}, O_RDONLY | O_NOFOLLOW) or die "open intent directory: $!\n";
    $dir->sync or die "sync intent directory: $!\n";
    close $dir;
    log_event($cfg, $enabled ? 'persistent intent enabled' : 'persistent intent disabled');
}

sub identity {
    my ($cfg, $timeout) = @_;
    return $cfg->{identity}->($timeout // 2) if $cfg->{identity};
    checked_directory($cfg->{runtime}, $cfg->{uid});
    die "unsafe runtime permissions\n" unless ((lstat $cfg->{runtime})[2] & 0777) == 0700;
    my @s = lstat "$cfg->{runtime}/control.sock";
    die "unsafe/missing service IPC socket\n" unless @s && S_ISSOCK($s[2]) && $s[4] == $cfg->{uid} && ($s[2] & 0777) == 0600;
    my $answer = '';
    local $SIG{ALRM} = sub { die "service IPC deadline expired\n"; };
    eval {
        Time::HiRes::alarm($timeout // 2);
        socket(my $socket, AF_UNIX, SOCK_STREAM, 0) or die "socket: $!\n";
        connect($socket, sockaddr_un("$cfg->{runtime}/control.sock")) or die "connect: $!\n";
        syswrite($socket, "identity\n") == 9 or die "write identity: $!\n";
        while (1) {
            my $n = sysread($socket, my $buffer, 1024);
            die "read identity: $!\n" unless defined $n;
            last unless $n;
            $answer .= $buffer;
            die "oversized identity reply\n" if length($answer) > 4096;
        }
        close $socket;
        Time::HiRes::alarm(0);
        1;
    } or do { my $error = $@; Time::HiRes::alarm(0); die $error; };
    return parse_identity($answer);
}

sub parse_identity {
    my ($answer) = @_;
    die "invalid/version-incompatible service identity response\n"
        unless $answer =~ /\AOK\nprotocol=(\d+)\nversion=([0-9.]+)\npid=(\d+)\nservice=(true|false)\nready=(true|false)\ninstance=([a-fA-F0-9-]{1,96})\n\z/;
    return {protocol => $1, version => $2, pid => $3, service => $4 eq 'true', ready => $5 eq 'true', instance => $6};
}

sub valid_service {
    my ($identity) = @_;
    return $identity && ($identity->{protocol} // 0) == 2
        && ($identity->{version} // '') eq $VERSION && $identity->{service}
        && ($identity->{instance} // '') =~ /\A[a-fA-F0-9-]{1,96}\z/;
}

# Enable uses one monotonic deadline across launchctl and live IPC. Separate
# per-command caps cannot add a second full readiness window after kickstart.
sub monotonic_now {
    my ($cfg) = @_;
    return $cfg->{monotonic_now}->() if $cfg->{monotonic_now};
    return clock_gettime(CLOCK_MONOTONIC);
}

sub enable_remaining {
    my ($cfg, $deadline, $cap) = @_;
    my $left = $deadline - monotonic_now($cfg);
    die "enable overall deadline expired; inspect status.sh and use disable.sh to verify automatic ownership\n"
        unless $left > 0;
    return $left < $cap ? $left : $cap;
}

sub enable {
    my ($cfg) = @_;
    check_parents($cfg);
    receipt($cfg);
    my $budget = $cfg->{ready_timeout} // 120;
    my $deadline = monotonic_now($cfg) + $budget;
    my $registered = defined loaded($cfg, enable_remaining($cfg, $deadline, 3));
    if ($registered) {
        die "service is registered with disabled intent; run disable.sh before enabling again\n"
            unless marker($cfg);
        # A clean `m1fan stop` leaves the job registered. Start that stopped
        # job without killing an already running instance: never pass -k.
        require_command($cfg, enable_remaining($cfg, $deadline, 5), $cfg->{launchctl}, 'enable', "system/$LABEL");
        # Budget for the configured 60s spawn throttle plus margin. The manual
        # gives no immediate-return guarantee for the kickstart client.
        require_command($cfg, enable_remaining($cfg, $deadline, 75), $cfg->{launchctl}, 'kickstart', "system/$LABEL");
        note($cfg, "Service is registered and enabled; start requested, checking live readiness.\n");
    } else {
        my $lock = try_lock($cfg) or die "another controller is active; complete stop/Ctrl+C before enabling\n";
        close $lock;
        set_marker($cfg, 1);
        my $ok = eval {
            require_command($cfg, enable_remaining($cfg, $deadline, 5), $cfg->{launchctl}, 'enable', "system/$LABEL");
            require_command($cfg, enable_remaining($cfg, $deadline, 10), $cfg->{launchctl}, 'bootstrap', 'system', $cfg->{plist});
            1;
        };
        if (!$ok) {
            my $error = $@;
            set_marker($cfg, 0);
            eval { require_command($cfg, 5, $cfg->{launchctl}, 'disable', "system/$LABEL") };
            die "enable failed; persistent intent is disabled. Inspect status and run disable.sh to confirm recovery.\n$error$@";
        }
    }
    my ($last, $instance, $consistent) = ('no compatible ready IPC reply', '', 0);
    my $next_registration_check = 0;
    while (monotonic_now($cfg) < $deadline) {
        if (monotonic_now($cfg) >= $next_registration_check) {
            # Registration is a periodic diagnostic; IPC supplies live identity.
            my $description = loaded($cfg, enable_remaining($cfg, $deadline, 3));
            die "service disappeared during enable; inspect management log and daemon.log\n" unless defined $description;
            $next_registration_check = monotonic_now($cfg) + 5;
        }
        my $identity = eval { identity($cfg, enable_remaining($cfg, $deadline, 2)) };
        if (valid_service($identity) && $identity->{ready}) {
            $consistent = $identity->{instance} eq $instance ? $consistent + 1 : 1;
            $instance = $identity->{instance};
            if ($consistent >= 3 && monotonic_now($cfg) <= $deadline) {
                note($cfg, "Service enabled and ready (version $VERSION, instance $instance); starts in system domain on future boots.\n");
                return;
            }
        } else {
            $consistent = 0;
            $last = $@ || 'IPC not ready or protocol/version/service mismatch';
        }
        my $left = $deadline - monotonic_now($cfg);
        last unless $left > 0;
        my $pause = $cfg->{poll_interval} // 0.2;
        $pause = $left if $pause > $left;
        if ($cfg->{pause}) { $cfg->{pause}->($pause); } else { sleep($pause); }
    }
    die "service remains enabled and registered; readiness not confirmed within ${budget}s: $last. Inspect status.sh; use disable.sh to stop and verify automatic ownership.\n";
}

sub activate { return enable(@_); }

sub wait_control_lock {
    my ($cfg, $seconds) = @_;
    my $deadline = time + $seconds;
    while (1) {
        my $lock = try_lock($cfg);
        return $lock if $lock;
        return undef if time >= $deadline;
        sleep($cfg->{poll_interval} // 0.05);
    }
}

sub disable {
    my ($cfg) = @_;
    check_parents($cfg);
    receipt($cfg);
    # Persist first: a concurrent or delayed crash restart must observe disabled
    # before any acquisition/write, even if a subsequent launchctl call fails.
    set_marker($cfg, 0);
    my @problems;
    eval { require_command($cfg, 5, $cfg->{launchctl}, 'disable', "system/$LABEL") };
    push @problems, "launchctl disable: $@" if $@;
    my $lock = try_lock($cfg);
    my $stop_error = '';
    if (!$lock) {
        my $response = eval { require_command($cfg, 35, "$cfg->{app}/m1fan", 'stop') };
        $stop_error = $@;
        note($cfg, $response) if defined $response;
        log_event($cfg, "initial IPC stop failed: $stop_error") if $stop_error;
    }
    my $registered = eval { loaded($cfg) };
    if ($@) {
        push @problems, "registration query: $@";
    } elsif (defined $registered) {
        eval {
            require_command($cfg, 10, $cfg->{launchctl}, 'bootout', "system/$LABEL");
            my $deadline = time + ($cfg->{shutdown_timeout} // 35);
            while (defined loaded($cfg)) {
                die "bootout was not confirmed within the shutdown window\n" if time >= $deadline;
                sleep($cfg->{poll_interval} // 0.5);
            }
        };
        push @problems, "bootout: $@" if $@;
    }
    $lock //= wait_control_lock($cfg, $cfg->{shutdown_timeout} // 35);
    if (!$lock) {
        die "persistent intent is disabled, but a controller still owns the control lock; preserving installation.\n$stop_error" . join('', @problems);
    }
    my $automatic = eval { require_command($cfg, 10, "$cfg->{app}/m1fan", 'verify-auto') };
    if ($@) {
        log_event($cfg, "automatic ownership initially unverified: $@");
        # auto obtains the same ControlLock itself. Keep the management lock and
        # disabled marker throughout this handoff; never operate without a lock.
        close $lock;
        undef $lock;
        my $restoration = eval { require_command($cfg, 40, "$cfg->{app}/m1fan", 'auto') };
        if ($@) { die "automatic restoration failed; persistent intent is disabled and installation retained.\n$@" . join('', @problems); }
        note($cfg, $restoration);
        $lock = wait_control_lock($cfg, 3)
            or die "automatic restoration client returned, but control lock is occupied; installation retained\n";
        $automatic = eval { require_command($cfg, 10, "$cfg->{app}/m1fan", 'verify-auto') };
        if ($@) { die "supplemental automatic restoration could not be verified; installation retained.\n$@"; }
        note($cfg, "Initial automatic verification failed; supplemental restoration is verified.\n");
    }
    note($cfg, $automatic);
    note($cfg, "Initial IPC recovery failed; subsequent automatic ownership verification passed.\n") if $stop_error;
    die "automatic ownership is verified, but launchd removal/disable is unconfirmed; installation retained.\n" . join('', @problems) if @problems;
    note($cfg, "Service disabled, unregistered, and automatic ownership verified.\n");
    return $lock;
}

sub deactivate { return disable(@_); }

sub status {
    my ($cfg) = @_;
    check_parents($cfg);
    my $installed = lstat("$cfg->{app}/installation.json") ? 1 : 0;
    if (!$installed && (lstat($cfg->{app}) || lstat($cfg->{plist}))) {
        die "Installed: unknown (partial/unrecognized files); preserve and inspect\n";
    }
    receipt($cfg) if $installed;
    note($cfg, 'Installed: ' . ($installed ? "yes ($VERSION)" : 'no') . "\n");
    note($cfg, 'Enabled intent: ' . ($installed ? (marker($cfg) ? 'yes' : 'no') : 'n/a') . "\n");
    my $registered = eval { loaded($cfg) };
    my $registration_error = $@;
    note($cfg, 'Registered: ' . ($registration_error ? "unknown ($registration_error)" : defined($registered) ? 'yes' : 'no') . "\n");
    # print-disabled is a diagnostic, never a machine contract. Expose raw output
    # separately; installed intent and launchd's persistent override can diverge.
    my ($override_code, $override) = command($cfg, 3, $cfg->{launchctl}, 'print-disabled', 'system');
    note($cfg, "launchd override diagnostic (exit=$override_code):\n$override\n");
    my $identity = eval { identity($cfg) };
    my $ipc_error = $@;
    if (valid_service($identity)) {
        note($cfg, "Live IPC: service=true protocol=2 version=$identity->{version} ready=" . ($identity->{ready} ? 'true' : 'false') . " instance=$identity->{instance} pid=$identity->{pid}\n");
        my ($code, $snapshot) = command($cfg, 3, "$cfg->{app}/m1fan", 'daemon-status');
        note($cfg, "Controller snapshot (exit=$code):\n$snapshot\n");
    } else {
        note($cfg, "Live IPC: unavailable or incompatible" . ($ipc_error ? " ($ipc_error)" : '') . "\n");
    }
    die "launchd registration status is unknown; no state change performed\n" if $registration_error;
}

sub uninstall {
    my ($cfg) = @_;
    my $lock = disable($cfg);
    my $receipt = receipt($cfg); # Recheck exact owned files before removing any.
    private_directory($cfg->{history}, $cfg->{uid});
    my $archive = "$cfg->{history}/uninstalled-" . int(time * 1000000) . "-$$.json";
    create_file($archive, JSON::PP->new->canonical->pretty->encode($receipt), 0600, $cfg->{uid});
    unlink($cfg->{plist}) == 1 or die "remove plist: $!\n";
    for my $name (@PAYLOAD, 'service.enabled', 'installation.json') {
        unlink("$cfg->{app}/$name") == 1 or die "remove $name: $!\n";
    }
    rmdir $cfg->{app}; # Preserve unrelated files, historical receipts and logs.
    note($cfg, "Managed installation removed; project evidence, runtime logs and $archive retained.\n");
}

sub dispatch {
    my ($cfg, $action, @arguments) = @_;
    check_parents($cfg);
    my $management = management_lock($cfg);
    private_directory($cfg->{history}, $cfg->{uid});
    my $transaction = "$cfg->{history}/transaction-" . int(time * 1000000) . "-$$.log";
    create_file($transaction, "M1FanControl $VERSION deployment action=$action\n", 0600, $cfg->{uid});
    local $cfg->{transaction_fh} = checked_open($transaction, $cfg->{uid}, O_WRONLY | O_APPEND);
    note($cfg, "Management transaction: $transaction\n");
    my $ok = eval {
        if ($action eq 'install') { die "two install arguments required\n" unless @arguments == 2; install($cfg, @arguments); }
        elsif ($action eq 'enable' || $action eq 'activate') { die "no arguments accepted\n" if @arguments; enable($cfg); }
        elsif ($action eq 'disable' || $action eq 'deactivate') { die "no arguments accepted\n" if @arguments; disable($cfg); }
        elsif ($action eq 'status') { die "no arguments accepted\n" if @arguments; status($cfg); }
        elsif ($action eq 'uninstall') { die "no arguments accepted\n" if @arguments; uninstall($cfg); }
        else { die "unknown deployment operation\n"; }
        1;
    };
    if (!$ok) { my $error = $@; log_event($cfg, "FAILED: $error"); die $error; }
    log_event($cfg, 'SUCCESS');
    $cfg->{transaction_fh}->sync or die "sync deployment transaction: $!\n";
    return 0;
}

sub main {
    my ($source, $action, @arguments) = @_;
    die "run the reviewed deployment script with sudo\n" unless $> == 0;
    umask 0077;
    return dispatch(production($source), $action, @arguments);
}
1;
