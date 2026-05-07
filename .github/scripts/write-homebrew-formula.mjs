#!/usr/bin/env node
'use strict';

import fs from 'node:fs';

const version = process.env.VERSION;
const macSha = process.env.MAC_SHA;
const linuxSha = process.env.LINUX_SHA;
const out = process.env.OUT || 'engram.rb';

function requireValue(name, value) {
  if (!value || !value.trim()) {
    console.error(`${name} is required`);
    process.exit(1);
  }
  return value.trim();
}

const v = requireValue('VERSION', version);
const mac = requireValue('MAC_SHA', macSha);
const linux = requireValue('LINUX_SHA', linuxSha);

const formula = `class Engram < Formula
  desc "Local-first causal index over code history"
  homepage "https://github.com/clickety-clacks/engram"
  version "${v}"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/clickety-clacks/engram/releases/download/v${v}/engram-aarch64-apple-darwin"
      sha256 "${mac}"
    else
      odie "Engram does not publish an x86_64 macOS binary yet. Add a source formula or release asset first."
    end
  end

  on_linux do
    if Hardware::CPU.intel? && Hardware::CPU.is_64_bit?
      url "https://github.com/clickety-clacks/engram/releases/download/v${v}/engram-x86_64-unknown-linux-gnu"
      sha256 "${linux}"
    else
      odie "Engram does not publish a Linux binary for this CPU yet. Add a source formula or release asset first."
    end
  end

  def install
    bin.install Dir["engram-*"].first => "engram"
    chmod 0755, bin/"engram"
  end

  test do
    assert_match "Engram indexes agent conversations", shell_output("#{bin}/engram --help")
  end
end
`;

fs.writeFileSync(out, formula);
console.log(`Wrote ${out}`);
