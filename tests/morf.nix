# Adapter gate accepts an independently built upstream Morf package. Generic
# Prek builds have no Morf dependency; callers select the reviewed adapter build.
{ pkgs, package, morf }:
import ./compositor.nix { inherit pkgs package morf; }
