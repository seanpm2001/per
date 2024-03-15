// Copyright (C) 2024 Lavra Holdings Limited - All Rights Reserved
pragma solidity ^0.8.0;

interface ExpressRelayFeeReceiver {
    function receiveAuctionProceedings(
        bytes calldata permissionKey
    ) external payable;
}