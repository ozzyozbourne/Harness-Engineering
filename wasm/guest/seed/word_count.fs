\ Seed tool `word_count`: {"text": string} -> number of whitespace-separated words.
: skip-space ( a u -- a' u' )  begin dup 0> if over c@ bl <= else false then while 1 /string repeat ;
: skip-word  ( a u -- a' u' )  begin dup 0> if over c@ bl >  else false then while 1 /string repeat ;
: count-words ( a u -- n )
  0 >r
  begin skip-space dup 0> while  r> 1+ >r  skip-word  repeat
  2drop r> ;
: run ( json-addr json-len -- out-addr out-len )
  s" text" json-get count-words n>s ;
