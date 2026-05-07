NAME="mailbox-isolation"
DESC="actor B receives 'B' message even while actor A is processing a flood — per-actor mailboxes are independent"
EXPECT="B"
EXPECT_TIMES=1
LANGS="lake go rust"
TIMEOUT=3
