# Homebrew formula. To use it, put this file in a tap (a GitHub repo named
# homebrew-<something> with this file under Formula/), or install straight from the repo.
# Before the first release: tag it, then fill in `url` and `sha256`:
#   curl -L https://github.com/you/dirrcmp/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256
class Dirrcmp < Formula
  desc "Terminal UI for comparing two directories (Rust rewrite of ilyasturki/dircmp)"
  homepage "https://github.com/you/dirrcmp"
  url "https://github.com/you/dirrcmp/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_SHA256_OF_THE_TARBALL"
  license "MIT"
  head "https://github.com/you/dirrcmp.git", branch: "main"

  depends_on "rust" => :build

  def install
    # Cargo.lock must be committed (cargo generate-lockfile) because Homebrew builds --locked.
    system "cargo", "install", *std_cargo_args
  end

  test do
    (testpath/"a").mkpath
    (testpath/"b").mkpath
    (testpath/"a/f.txt").write "one\n"
    (testpath/"b/f.txt").write "one\n"
    system bin/"dirrcmp", "check", testpath/"a", testpath/"b"
    (testpath/"b/f.txt").write "two\n"
    shell_output("#{bin}/dirrcmp check #{testpath}/a #{testpath}/b", 1) # exit status 1: different
    assert_match "f.txt", shell_output("#{bin}/dirrcmp diff #{testpath}/a #{testpath}/b")
  end
end
