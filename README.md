# VBoard
A vosk based voice to text for linux, specifically made for Debian based phosh arm64 devices.
Supports multilingual, and was built with a furiphone flx1 in mind.

built with rust, I hate rust 

You will need vosk api to build this, you can find it on github or you can use the one included in my release package.


Features:
* Voice to text 
* Multilingual (English, German, Russian, Chinese, Japanese, Dutch, Swedish, French, Spanish, Italian, Polish, Korean)
* Start on boot - starts on boot
* Auto capitalisation options
* Auto Punctuation options
* Ram saver (presently sometimes broken, unloads and loads model dynmically to save ram when you aren't typing) 
* NLP Grammar correction for German and English (implemented from https://github.com/bminixhofer/nlprule)
* Mic enhancements for the model to hear you better
* Auto hides when keyboard is closed, if keyboard is open it reappears (can be disabled with always show button) 
