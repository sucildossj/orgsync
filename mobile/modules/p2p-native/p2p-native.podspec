require "json"
package = JSON.parse(File.read(File.join(__dir__, "package.json")))

Pod::Spec.new do |s|
  s.name         = "p2p-native"
  s.version      = package["version"]
  s.summary      = package["description"]
  s.license      = "MIT"
  s.authors      = { "OrgSync" => "noreply@example.com" }
  s.homepage     = "https://example.com/orgsync"
  s.platforms    = { :ios => "15.1" }
  s.source       = { :path => "." }

  # Our thin bridge plus the uniffi-generated Swift, which
  # scripts/build-ios.sh writes into ios/generated.
  s.source_files = "ios/**/*.{h,m,mm,swift}"

  # The Rust core, built for device and simulator by the same script.
  s.vendored_frameworks = "ios/P2PMobileFFI.xcframework"

  s.dependency "React-Core"
end
