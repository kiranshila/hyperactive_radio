.program i2s_in
sync_l:
    wait 0 pin 2                ; wait for LRCK low (left channel)
    set y, 1                    ; y == 1 if we're writing the L channel
pause:
    wait 0 pin 1                ; skip one BCK cycle (I2S requires it)
    wait 1 pin 1
    set x, 15                   ; 16 bits per channel (x is the down counter)
write:
    wait 0 pin 1                ; now clear to change output
    out pins, 1                 ; send data bit
    wait 1 pin 1                ; wait for bit to be read
    jmp x-- write               ; keep writing while we have bits

    ;; at this point, we've written 16 of the expected 24 bits to the DAC, so
    ;; just pad the rest with zeros; the next LRCK sync will take care of
    ;; re-aligning us to the DAC clocks.
    wait 0 pin 1                ; wait for BCK low
    set pins, 0

    jmp !y sync_l               ; if we just did R, go do L, else... else
sync_r:
    wait 1 pin 2                ; wait for LRCK high (right channel)
    set y, 0                    ; y == 0 if we're writing the R channel
    jmp pause
