use strict;
use warnings;
no warnings 'once';
use FindBin qw($RealBin);
use File::Temp qw(tempdir);
use Digest::SHA qw(sha256_hex);
use Test::More;
use Time::HiRes qw(time);
require "$RealBin/../ServiceInstall.pm";

sub fixture {
    my $directory = tempdir('m1fan-deploy-XXXXXX', TMPDIR => 1, CLEANUP => 1);
    my $base = "$directory/Application Support";
    mkdir $base, 0700 or die $!;
    my $loaded = 0;
    my @calls;
    my $cfg = {uid => $>, source => "$RealBin/..", parents => [$base], app => "$base/M1FanControl",
        plist => "$base/local.m1fan.control.plist", lock => "$base/control.lock", runtime => "$base/runtime",
        management_lock => "$base/management.lock", history => "$base/history",
        launchctl => '/mock/launchctl', ready_timeout => 0.03, shutdown_timeout => 0.01, poll_interval => 0.001};
    $cfg->{runner} = sub {
        my ($timeout, @args) = @_;
        push @calls, [@args];
        if ($args[1] eq 'print') { return $loaded ? (0, "opaque launchctl diagnostic without PID\n") : (113, 'Could not find service "local.m1fan.control" in domain for system'); }
        if ($args[1] eq 'bootstrap') { $loaded = 1; return (0, ''); }
        if ($args[1] eq 'bootout') { $loaded = 0; return (0, ''); }
        if ($args[1] eq 'verify-auto') { return (0, "automatic ownership verified\n"); }
        if ($args[1] eq 'print-disabled') { return (0, 'mock persistent launchd override diagnostic'); }
        return (0, '');
    };
    my $source = "$base/source-binary";
    open my $f, '>', $source or die $!; print {$f} 'mock binary bytes'; close $f;
    return ($cfg, $source, sha256_hex('mock binary bytes'), \$loaded, \@calls);
}
sub ready_identity { return {protocol => 2, version => '0.5.0', pid => 1234, service => 1, ready => 1, instance => 'ab-cd'}; }
sub read_file { open my $file, '<', $_[0] or die $!; local $/; return <$file>; }

subtest 'generated template equals file and has bounded launchd retry settings' => sub {
    is(M1FanDeploy::plist(), read_file("$RealBin/../local.m1fan.control.plist"));
    like(M1FanDeploy::plist(), qr/SuccessfulExit<\/key><false\//);
    like(M1FanDeploy::plist(), qr/ThrottleInterval<\/key><integer>60<\/integer>/);
    like(M1FanDeploy::plist(), qr/ExitTimeOut<\/key><integer>30<\/integer>/);
};

subtest 'hash mismatch publishes nothing' => sub {
    my ($cfg, $source) = fixture();
    eval { M1FanDeploy::install($cfg, $source, '0' x 64) };
    like($@, qr/SHA-256 does not match/);
    ok(!-e $cfg->{app}); ok(!-e $cfg->{plist});
};

subtest 'install disabled, source pinned, protected receipt and spaced paths' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::dispatch($cfg, 'install', $source, $hash);
    is((stat "$cfg->{app}/m1fan")[2] & 0777, 0755);
    is((stat $cfg->{plist})[2] & 0777, 0644);
    is((stat "$cfg->{app}/installation.json")[2] & 0777, 0600);
    is((stat "$cfg->{app}/service.enabled")[2] & 0777, 0600);
    is(M1FanDeploy::receipt($cfg)->{source_sha256}, $hash);
    is(M1FanDeploy::receipt($cfg)->{schema}, 2);
    is(M1FanDeploy::marker($cfg), 0);
    is($$loaded, 0);
    ok(grep($_->[1] eq 'disable', @$calls));
    ok(!grep($_->[1] eq 'bootstrap', @$calls));
    opendir my $history, $cfg->{history} or die $!;
    my @logs = grep { /transaction-.*\.log/ } readdir $history;
    is(scalar(@logs), 1);
    like(read_file("$cfg->{history}/$logs[0]"), qr/SUCCESS/);
};

subtest 'management mutex serializes install enable disable and uninstall' => sub {
    my ($cfg, $source, $hash) = fixture();
    my $held = M1FanDeploy::management_lock($cfg);
    for my $action (qw(install enable disable uninstall)) {
        eval { M1FanDeploy::dispatch($cfg, $action, $source, $hash) };
        like($@, qr/management lock/);
    }
    ok(!-e $cfg->{app});
};

subtest 'active controller blocks installation' => sub {
    my ($cfg, $source, $hash) = fixture();
    my $held = M1FanDeploy::try_lock($cfg);
    eval { M1FanDeploy::install($cfg, $source, $hash) };
    like($@, qr/controller owns the control lock/);
    ok(!-e $cfg->{app});
};

subtest 'symlinks and writable directories are rejected' => sub {
    my ($cfg, $source, $hash) = fixture();
    my $outside = "$cfg->{parents}[0]/outside";
    mkdir $outside, 0700 or die $!;
    symlink $outside, $cfg->{app} or die $!;
    eval { M1FanDeploy::install($cfg, $source, $hash) };
    like($@, qr/unsafe directory/);
    ok(!-e "$outside/m1fan");
    unlink $cfg->{app};
    chmod 0777, $cfg->{parents}[0];
    eval { M1FanDeploy::install($cfg, $source, $hash) };
    like($@, qr/unsafe directory/);
};

subtest 'hidden unique files in spaced application directory are preserved' => sub {
    my ($cfg, $source, $hash) = fixture();
    mkdir $cfg->{app}, 0700 or die $!;
    open my $file, '>', "$cfg->{app}/.unique" or die $!; print {$file} 'preserve'; close $file;
    eval { M1FanDeploy::install($cfg, $source, $hash) };
    like($@, qr/directory must be empty/);
    ok(-f "$cfg->{app}/.unique"); ok(!-e "$cfg->{app}/m1fan");
};

subtest 'enable accepts protocol identity independently of launchctl output structure' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $samples = 0;
    $cfg->{identity} = sub { ++$samples; return ready_identity(); };
    M1FanDeploy::enable($cfg);
    is($$loaded, 1);
    is(M1FanDeploy::marker($cfg), 1);
    is($samples, 3);
    M1FanDeploy::enable($cfg);
    is($samples, 6, 'idempotent enable also confirms three stable ready replies');
    ok(grep($_->[1] eq 'enable', @$calls));
    ok(grep($_->[1] eq 'bootstrap' && $_->[2] eq 'system' && $_->[3] eq $cfg->{plist}, @$calls));
};

subtest 'enable starts a cleanly stopped registered service' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $running = 0;
    my $starts = 0;
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub {
        my ($t, @args) = @_;
        if ($args[1] eq 'stop') { $running = 0; return (0, "automatic restoration verified\n"); }
        if ($args[1] eq 'bootstrap' || $args[1] eq 'kickstart') {
            if (!$running) { ++$starts; $running = 1; }
        }
        return $runner->($t, @args);
    };
    $cfg->{identity} = sub {
        die "stopped service has no live IPC\n" unless $running;
        my $id = ready_identity(); $id->{instance} = "ab-$starts"; return $id;
    };
    M1FanDeploy::enable($cfg);
    M1FanDeploy::require_command($cfg, 35, "$cfg->{app}/m1fan", 'stop');
    is($$loaded, 1, 'clean stop retains launchd registration');
    is(M1FanDeploy::marker($cfg), 1, 'clean stop retains enabled intent');
    is($running, 0);
    M1FanDeploy::enable($cfg);
    is($running, 1); is($starts, 2);
    my @kickstarts = grep { $_->[1] eq 'kickstart' } @$calls;
    is(scalar @kickstarts, 1);
    is_deeply($kickstarts[0], ['/mock/launchctl', 'kickstart', 'system/local.m1fan.control']);
    is(scalar(grep { $_->[1] eq 'bootstrap' } @$calls), 1, 'no duplicate bootstrap');
};

subtest 'enable leaves an already running service instance intact' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $instance = 'ab-1';
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub {
        my ($t, @args) = @_;
        $instance = 'ab-2' if $args[1] eq 'kickstart' && grep { $_ eq '-k' } @args;
        return $runner->($t, @args);
    };
    $cfg->{identity} = sub { my $id = ready_identity(); $id->{instance} = $instance; return $id; };
    M1FanDeploy::enable($cfg);
    M1FanDeploy::enable($cfg);
    is($instance, 'ab-1');
    ok(grep($_->[1] eq 'kickstart' && @$_ == 3, @$calls));
    ok(!grep($_ eq '-k', map { @$_ } @$calls));
};

subtest 'kickstart budget covers 60 second throttle and succeeds after delayed reply' => sub {
    my ($cfg, $source, $hash, $loaded) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    M1FanDeploy::set_marker($cfg, 1);
    $$loaded = 1;
    delete $cfg->{ready_timeout}; # Exercise production 120-second total budget.
    my $now = 0;
    $cfg->{monotonic_now} = sub { return $now; };
    $cfg->{pause} = sub { $now += $_[0]; };
    my $runner = $cfg->{runner};
    my $kickstart_budget;
    $cfg->{runner} = sub {
        my ($t, @args) = @_;
        if ($args[1] eq 'kickstart') { $kickstart_budget = $t; $now += 70; }
        return $runner->($t, @args);
    };
    $cfg->{identity} = sub { return ready_identity(); };
    M1FanDeploy::enable($cfg);
    is($kickstart_budget, 75, 'client cap spans throttle with margin');
    cmp_ok($kickstart_budget, '>', 60);
    cmp_ok($now, '>=', 70);
    cmp_ok($now, '<', 120, 'IPC completion shares total deadline');
};

subtest 'kickstart time is deducted from the single enable readiness budget' => sub {
    my ($cfg, $source, $hash, $loaded) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    M1FanDeploy::set_marker($cfg, 1);
    $$loaded = 1;
    delete $cfg->{ready_timeout};
    my $now = 0;
    $cfg->{monotonic_now} = sub { return $now; };
    $cfg->{pause} = sub { $now += $_[0]; };
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub {
        my ($t, @args) = @_;
        $now += 70 if $args[1] eq 'kickstart';
        return $runner->($t, @args);
    };
    my @ipc_budgets;
    $cfg->{identity} = sub {
        my ($budget) = @_;
        push @ipc_budgets, $budget;
        $now += $budget;
        my $value = ready_identity(); $value->{ready} = 0; return $value;
    };
    eval { M1FanDeploy::enable($cfg) };
    like($@, qr/readiness not confirmed within 120s/);
    cmp_ok($now, '<=', 120.000001, 'does not add a fresh 120s window after kickstart');
    cmp_ok($now, '>=', 119.99);
    cmp_ok($ipc_budgets[-1], '<', 2, 'last IPC call uses remaining fraction');
    is(M1FanDeploy::marker($cfg), 1, 'timeout reports existing enabled intent');
};

subtest 'enable restarts consecutive readiness count when instance changes' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $samples = 0;
    $cfg->{identity} = sub { my $value = ready_identity(); $value->{instance} = ++$samples <= 2 ? 'a' : 'b'; return $value; };
    M1FanDeploy::enable($cfg);
    is($samples, 5);
};

subtest 'enable rejects manual workers and incompatible protocol/version' => sub {
    for my $key ('service', 'version', 'protocol') {
        my ($cfg, $source, $hash) = fixture();
        M1FanDeploy::install($cfg, $source, $hash);
        $cfg->{identity} = sub { my $identity = ready_identity(); $identity->{$key} = $key eq 'version' ? '0.4.0' : 0; return $identity; };
        eval { M1FanDeploy::enable($cfg) };
        like($@, qr/readiness not confirmed/, $key);
    }
};

subtest 'failed bootstrap restores persistent disabled intent' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub { my ($t, @args) = @_; return (1, 'bootstrap rejected') if $args[1] eq 'bootstrap'; return $runner->($t, @args); };
    eval { M1FanDeploy::enable($cfg) };
    like($@, qr/bootstrap rejected/);
    is(M1FanDeploy::marker($cfg), 0);
};

subtest 'disable persists intent before launchctl and stop' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    M1FanDeploy::set_marker($cfg, 1);
    $$loaded = 1;
    my $held = M1FanDeploy::try_lock($cfg);
    my $runner = $cfg->{runner};
    my @events;
    $cfg->{runner} = sub {
        my ($t, @args) = @_;
        is(M1FanDeploy::marker($cfg), 0, "disabled before $args[1]");
        push @events, $args[1];
        if ($args[1] eq 'stop') { close $held; return (0, "automatic restoration verified\n"); }
        return $runner->($t, @args);
    };
    my $lock = M1FanDeploy::disable($cfg);
    is($events[0], 'disable'); is($events[1], 'stop');
    is($$loaded, 0);
    ok($lock);
};

subtest 'crash-left manual state receives locked auto and subsequent verification' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $runner = $cfg->{runner};
    my $verified = 0;
    my $auto_calls = 0;
    $cfg->{runner} = sub {
        my ($t, @args) = @_;
        if ($args[1] eq 'verify-auto') { return ++$verified == 1 ? (1, 'mode=manual') : (0, "automatic verified\n"); }
        if ($args[1] eq 'auto') {
            ++$auto_calls;
            my $lock = M1FanDeploy::try_lock($cfg);
            ok($lock, 'auto can obtain control lock itself');
            is(M1FanDeploy::marker($cfg), 0);
            return (0, "supplemental restoration verified\n");
        }
        return $runner->($t, @args);
    };
    my $lock = M1FanDeploy::disable($cfg);
    is($verified, 2); is($auto_calls, 1); ok($lock);
};

subtest 'unrecoverable manual state retains installation and disabled intent' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub { my ($t, @args) = @_; return (1, 'hardware restoration failure') if $args[1] eq 'verify-auto' || $args[1] eq 'auto'; return $runner->($t, @args); };
    eval { M1FanDeploy::uninstall($cfg) };
    like($@, qr/automatic restoration failed/);
    ok(-f "$cfg->{app}/m1fan"); ok(-f $cfg->{plist});
    is(M1FanDeploy::marker($cfg), 0);
};

subtest 'IPC failure followed by confirmed recovery allows disable and records details' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $held = M1FanDeploy::try_lock($cfg);
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub { my ($t, @args) = @_; if ($args[1] eq 'stop') { close $held; return (1, 'initial automatic restoration FAILED'); } return $runner->($t, @args); };
    M1FanDeploy::dispatch($cfg, 'disable');
    opendir my $history, $cfg->{history} or die $!;
    my @logs = grep { /transaction-.*\.log/ } readdir $history;
    my $log = read_file("$cfg->{history}/$logs[0]");
    like($log, qr/initial automatic restoration FAILED/);
    like($log, qr/subsequent automatic ownership verification passed/);
    like($log, qr/SUCCESS/);
};

subtest 'still-owned control lock preserves installation after unsuccessful stop' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $held = M1FanDeploy::try_lock($cfg);
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub { my ($t, @args) = @_; return (1, 'stop failed') if $args[1] eq 'stop'; return $runner->($t, @args); };
    eval { M1FanDeploy::uninstall($cfg) };
    like($@, qr/still owns the control lock/);
    ok(-f "$cfg->{app}/m1fan"); ok(-f $cfg->{plist});
    is(M1FanDeploy::marker($cfg), 0);
};

subtest 'disable query failure cannot be mistaken for successful removal' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    my $runner = $cfg->{runner};
    $cfg->{runner} = sub { my ($t, @args) = @_; return (1, 'unrecognized launchctl failure') if $args[1] eq 'print'; return $runner->($t, @args); };
    eval { M1FanDeploy::uninstall($cfg) };
    like($@, qr/removal\/disable is unconfirmed/);
    is(M1FanDeploy::marker($cfg), 0);
    ok(-f "$cfg->{app}/m1fan");
};

subtest 'uninstall preserves unrelated files, historical receipt and source' => sub {
    my ($cfg, $source, $hash, $loaded, $calls) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    $$loaded = 1;
    open my $other, '>', "$cfg->{app}/unrelated.txt" or die $!; print {$other} 'preserve'; close $other;
    M1FanDeploy::dispatch($cfg, 'uninstall');
    ok(!-e "$cfg->{app}/m1fan"); ok(!-e $cfg->{plist});
    ok(-f "$cfg->{app}/unrelated.txt"); ok(-f $source);
    is($$loaded, 0);
    ok(grep($_->[1] eq 'bootout' && $_->[2] eq 'system/local.m1fan.control' && @$_ == 3, @$calls));
    opendir my $history, $cfg->{history} or die $!;
    my @receipts = grep { /uninstalled-.*\.json/ } readdir $history;
    is(scalar(@receipts), 1);
    like(read_file("$cfg->{history}/$receipts[0]"), qr/\Q$hash\E/);
};

subtest 'changed installed binary is preserved' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    open my $file, '>>', "$cfg->{app}/m1fan" or die $!; print {$file} 'changed'; close $file;
    eval { M1FanDeploy::uninstall($cfg) };
    like($@, qr/differs from its receipt/);
    ok(-f "$cfg->{app}/m1fan"); ok(-f $cfg->{plist});
};

subtest 'known R5 and unknown receipts refuse migration without altering evidence' => sub {
    for my $known (0, 1) {
        my ($cfg, $source, $hash) = fixture();
        mkdir $cfg->{app}, 0755 or die $!;
        my $text = JSON::PP->new->encode({schema => $known ? 1 : 99, plist_sha256 => $known ? $M1FanDeploy::R5_PLIST_SHA : 'unknown'});
        M1FanDeploy::create_file("$cfg->{app}/installation.json", $text, 0600, $cfg->{uid});
        eval { M1FanDeploy::install($cfg, $source, $hash) };
        like($@, $known ? qr/R5 installation detected/ : qr/unknown installation receipt/);
        is(read_file("$cfg->{app}/installation.json"), $text);
        ok(!-e "$cfg->{app}/m1fan");
    }
};

subtest 'malformed symlink intent is rejected without changing target' => sub {
    my ($cfg, $source, $hash) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    unlink "$cfg->{app}/service.enabled";
    symlink $source, "$cfg->{app}/service.enabled";
    eval { M1FanDeploy::disable($cfg) };
    like($@, qr/open/);
    is(read_file($source), 'mock binary bytes');
};

subtest 'status distinguishes installed intent registration and versioned live IPC' => sub {
    my ($cfg, $source, $hash, $loaded) = fixture();
    M1FanDeploy::install($cfg, $source, $hash);
    $$loaded = 1;
    $cfg->{identity} = sub { return ready_identity(); };
    my $output = '';
    { open my $capture, '>', \$output; local *STDOUT = $capture; M1FanDeploy::status($cfg); }
    like($output, qr/Installed: yes \(0.5.0\)/);
    like($output, qr/Enabled intent: no/);
    like($output, qr/Registered: yes/);
    like($output, qr/Live IPC: service=true protocol=2/);
    like($output, qr/launchd override diagnostic/);
};

subtest 'unknown launchctl query failure is not treated as absent' => sub {
    my ($cfg, $source, $hash) = fixture();
    $cfg->{runner} = sub { return (1, 'permission denied'); };
    eval { M1FanDeploy::install($cfg, $source, $hash) };
    like($@, qr/cannot determine whether service is loaded/);
    ok(!-e $cfg->{app});
};

subtest 'versioned identity parser rejects old or oversized response' => sub {
    my $value = M1FanDeploy::parse_identity("OK\nprotocol=2\nversion=0.5.0\npid=1234\nservice=true\nready=true\ninstance=ab-cd\n");
    ok(M1FanDeploy::valid_service($value));
    eval { M1FanDeploy::parse_identity("OK\npid=1234\nservice=true\nready=true\n") };
    like($@, qr/invalid\/version-incompatible/);
};

subtest 'bounded command timeout affects only mock client' => sub {
    my $start = time;
    eval { M1FanDeploy::command({}, 0.05, '/bin/sleep', '5') };
    like($@, qr/exceeded/);
    cmp_ok(time - $start, '<', 2);
};

done_testing();
