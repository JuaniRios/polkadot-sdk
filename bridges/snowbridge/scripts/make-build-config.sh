#!/usr/bin/env bash

cd ../ethereum

truffle exec scripts/dumpParachainConfig.js | sed '/^Using/d;/^$/d'
