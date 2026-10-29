#!/usr/bin/env perl
# Writes synthetic touch gestures and key presses into an existing evdev node.
use strict;
use warnings;
use Fcntl qw(O_WRONLY);
use Time::HiRes qw(sleep);

my $usage = <<'HELP';
Usage: inject-input.pl [--size WIDTHxHEIGHT] DEVICE STEP...
Writes input events into DEVICE, an existing /dev/input/eventN node. The kernel
delivers them to the client grabbing that device, or else to all its readers,
exactly as it delivers the hardware's own reports. Steps run in order:
  swipe:FINGERS:DIRECTION[:X,Y[:MS]]
                                 one to five contacts land in one frame, move
                                 40% of the screen up, down, left or right in
                                 20 frames 15 ms apart, then lift; X,Y is the
                                 normalized start of their centre, by default
                                 placing the swipe around the screen centre;
                                 with MS the first contact moves alone that
                                 long before the others land beside it
  key:CODE                       press and release one key code
  wait:MS                        pause
--size sets the contact axis ranges of a DEVICE that cannot report them.
Each step prints its start on the monotonic clock in seconds.
HELP

use constant {
    EV_SYN             => 0,
    EV_KEY             => 1,
    EV_ABS             => 3,
    BTN_TOUCH          => 0x14a,
    ABS_X              => 0x00,
    ABS_Y              => 0x01,
    ABS_MT_SLOT        => 0x2f,
    ABS_MT_POSITION_X  => 0x35,
    ABS_MT_POSITION_Y  => 0x36,
    ABS_MT_TRACKING_ID => 0x39,
};
# BTN_TOOL_FINGER, DOUBLETAP, TRIPLETAP, QUADTAP and QUINTTAP by contact count.
my @tools = (0x145, 0x14d, 0x14e, 0x14f, 0x148);
my %directions = (up => [0, -1], down => [0, 1], left => [-1, 0], right => [1, 0]);

my ($width, $height);
if (@ARGV && $ARGV[0] eq '--size') {
    shift @ARGV;
    (shift(@ARGV) // '') =~ /^([1-9]\d*)x([1-9]\d*)$/ or die $usage;
    ($width, $height) = ([0, $1 - 1], [0, $2 - 1]);
}
my ($path, @steps) = @ARGV;
die $usage unless defined $path && @steps;
for (@steps) {
    die "Unknown step \"$_\".\n$usage"
        unless /^swipe:[1-5]:(up|down|left|right)(:(0(\.\d+)?|1),(0(\.\d+)?|1)(:\d+)?)?$/
        || (/^key:(\d+)$/ && $1 <= 0x2ff) || /^wait:\d+$/;
}
sysopen(my $device, $path, O_WRONLY) or die "$path: $!\n";

# EVIOCGABS fills struct input_absinfo: value, minimum, maximum, fuzz, flat, resolution.
sub range {
    my ($code) = @_;
    my $info = "\0" x 24;
    ioctl($device, 0x80184540 + $code, $info) or die "$path reports no axis $code ($!); pass --size.\n";
    my (undef, $minimum, $maximum) = unpack('l3', $info);
    return [$minimum, $maximum];
}
$width  //= range(ABS_MT_POSITION_X);
$height //= range(ABS_MT_POSITION_Y);

sub frame {
    my $bytes = join '', map { pack('l!2 S2 l', 0, 0, @$_) } @_, [EV_SYN, 0, 0];
    my $written = syswrite($device, $bytes);
    die "$path: $!\n" unless defined $written && $written == length $bytes;
}

sub at {
    my ($axis, $fraction) = @_;
    $fraction = $fraction < 0 ? 0 : $fraction > 1 ? 1 : $fraction;
    return int($axis->[0] + $fraction * ($axis->[1] - $axis->[0]) + 0.5);
}

my $tracking = 1 + $$ % 30000;

sub swipe {
    my ($fingers, $direction, $start, $late) = @_;
    my ($dx, $dy) = @{$directions{$direction}};
    my ($x, $y) = $start ? split(/,/, $start) : (0.5 - 0.2 * $dx, 0.5 - 0.2 * $dy);
    # Contacts sit 12% of the screen apart across the motion.
    my @contacts = map {
        my $offset = 0.12 * ($_ - ($fingers - 1) / 2);
        [$x + $offset * abs($dy), $y + $offset * abs($dx)]
    } 0 .. $fingers - 1;
    # Frames the first contact moves alone before the others land.
    my $lead = $late ? int(($late + 14) / 15) : 0;
    for my $step (0 .. 20 + $lead) {
        my $travel = 0.02 * $step;
        my @events;
        for my $slot (0 .. $#contacts) {
            my $landing = $slot ? $lead : 0;
            next if $step < $landing;
            my ($cx, $cy) = @{$contacts[$slot]};
            push @events, [EV_ABS, ABS_MT_SLOT, $slot];
            push @events, [EV_ABS, ABS_MT_TRACKING_ID, $tracking + $slot] if $step == $landing;
            push @events, [EV_ABS, ABS_MT_POSITION_X, at($width, $cx + $travel * $dx)],
                [EV_ABS, ABS_MT_POSITION_Y, at($height, $cy + $travel * $dy)];
        }
        push @events, [EV_KEY, BTN_TOUCH, 1], [EV_KEY, $tools[$lead ? 0 : $fingers - 1], 1]
            if $step == 0;
        push @events, [EV_KEY, $tools[0], 0], [EV_KEY, $tools[$fingers - 1], 1]
            if $lead && $step == $lead;
        push @events, [EV_ABS, ABS_X, at($width, $contacts[0][0] + $travel * $dx)],
            [EV_ABS, ABS_Y, at($height, $contacts[0][1] + $travel * $dy)];
        frame(@events);
        sleep 0.015;
    }
    frame((map { ([EV_ABS, ABS_MT_SLOT, $_], [EV_ABS, ABS_MT_TRACKING_ID, -1]) } 0 .. $#contacts),
        [EV_KEY, BTN_TOUCH, 0], [EV_KEY, $tools[$fingers - 1], 0]);
    $tracking += $fingers;
}

$| = 1;
for (@steps) {
    my ($kind, @arguments) = split /:/;
    printf "%.6f %s\n", Time::HiRes::clock_gettime(Time::HiRes::CLOCK_MONOTONIC()), $_;
    if ($kind eq 'swipe') {
        swipe(@arguments);
    } elsif ($kind eq 'key') {
        frame([EV_KEY, $arguments[0], 1]);
        sleep 0.08;
        frame([EV_KEY, $arguments[0], 0]);
    } else {
        sleep $arguments[0] / 1000;
    }
}
