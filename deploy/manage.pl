#!/usr/bin/perl
use strict;
use warnings;
use FindBin qw($RealBin);
require "$RealBin/ServiceInstall.pm";
my $action = shift @ARGV // '';
my $ok = eval { M1FanDeploy::main($RealBin, $action, @ARGV); 1 };
if (!$ok) { print STDERR "m1fan deploy: $@"; exit 1; }
