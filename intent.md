---

### Radiomusic Manager User Stories: 

 1. As a user, I want to detect my SD card, so that I can begin working on it. 
 2. As a user, I want to format an SD card for the musicthing radiomusic, so I can be sure it will work as expected. 
 3. As a user, I want to be able to supply and audio folder to draw qualified/eligible samples from, so that I can smoothly select and add them. 
 4. As a user, I want to filter eligible sounds by simple criteria like length and metadata, so I can design an interesting sound library. 
 5. As a user, I want to use my pool of filtered sounds to build a randomized SD card for the musicthing radiomusic. 
 6. As a user, I want to quickly choose how many folders to include on my sd card, so that I can have the desired configuration.
 7. As a user, I want the option to normalize all files used on my card, so that I can avoid dramatic differences in volumes. 
 8. As a user, I want to preview audio files before I assign them to a folder on the radiomusic card, so I can be sure about what I'm adding. 
 9. As a user, I want to define a start and end to audio samples before I load them on to the radiomusic card, so I can more gracefully handle large files without making numerous local copies. 
10. As a user, I want to read and update my settings file using a simple UI wizard as the second step, "Settings", after "Select Card", and before "Select Files"


## Non-functional requirements: 

 *  File writes should be optimized for speed and not correctness. This device is designed with chaos and randomness in mind, not precision and data safety.  
 *  If a user makes a destructive choice, and formatting the card is the fastest way to clear it, simply format the card. 
