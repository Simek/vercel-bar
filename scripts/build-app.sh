#!/bin/sh
set -eu

rm -rf build
xcodebuild -project VercelBar.xcodeproj -scheme VercelBar -configuration Release -derivedDataPath build build

app_dir="build/Build/Products/Release/VercelBar.app"
applications_app="/Applications/VercelBar.app"

rm -rf "$applications_app"
cp -R -v "$app_dir" "$applications_app"

echo "$app_dir -> $applications_app"

echo "Built and installed $applications_app"
