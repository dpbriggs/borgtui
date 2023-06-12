#!/bin/bash

# This is a hacky script to assist in automating borgtui AUR package updates.

cd /hdd2/programming/borgtui/archlinux/borgtui-git
git pull origin
makepkg -cf
makepkg --printsrcinfo > .SRCINFO
git add .
git commit -m "Automated regular package update"
git push origin
cd -
