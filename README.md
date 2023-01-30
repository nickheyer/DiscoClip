![image](https://user-images.githubusercontent.com/60236014/215372009-d6ca97db-f187-4c39-a8d9-d7ac31e5d52a.png)

# DiscoClip
A web-app/bot that monitors discord channels for IG/TikTok video-links then downloads, transcodes (to Discord's size limit), and posts the video to the same channel.

Running the discord bot requires a discord developer account (https://discord.com/developers/applications), and a bot created/invited (via your developer acount) to your chosen discord server.
<hr />

## Installation via Docker (Recommended)

### With Linux

```bash 
curl https://raw.githubusercontent.com/nickheyer/DiscoMon/main/auto_install_update.sh -o auto_install_update.sh && sudo bash auto_install_update.sh
```


-or- { (Docker required for the following installation methods)

Download Docker Image (x86_64 Architecture) 
```
docker image pull nickheyer/discomon:latest
```

Download Docker Image (aarch64 Architecture, ie: Raspberry-Pi) 
```
docker image pull nickheyer/discomon_rpi:latest
```

-after 'docker pull'-

Run Docker Container
```
docker run -d -p 6969:6969 nickheyer/discomon
```

}

- The server within the docker container can be accessed at [http://127.0.0.1:6969](http://127.0.0.1:6969)
<hr />



Fill in the required information by pressing the "edit" tab or within the json itself ("values" tab), start the bot by moving the switch labeled Bot I/O, profit. If you run into any errors, make sure all fields are completed in the edit tab. Make sure that you add your own "discordusername#1234" otherwise the bot will not respond. 

You can test that the bot is functional by typing '!dm test' into chat

![image](https://user-images.githubusercontent.com/60236014/193970329-f0b0a50f-9b88-4e0f-adfb-d136ddbbbd48.png)

## API

The information is sent as a JSON to whichever API endpoint is specified as the "url" in "request.json". You can configure the url, as well as any other parameters/headers in the "request" tab. 

Here is a sample request, make sure your endpoint can accept a POST request containing a JSON w/ these keys:
```
{
      'currentStatus': 
          {
          "spotifyArtist": "The Smashing Pumpkins",
          "spotifyAlbum": "Mellon Collie And The Infinite Sadness (Deluxe Edition)",
          "spotifyAlbumCoverUrl": "https://i.scdn.co/image/ab67616d0000b273431ac6e6f393acf475730ec6",
          "spotifyTitle": "Tonight, Tonight - Remastered 2012",
          "spotifyTrackUrl": "https://open.spotify.com/track/7bu0znpSbTks0O6I98ij0W",
          "activityName": "Visual Studio Code",
          "activityUrl": null,
          "activityState": "Workspace: DiscoMon",
          "activityType": "playing",
          "activityDetails": "Editing request.json"
          }
}
```
For more detailed information on the API, see my example API endpoint (currently live), provided via a Django server [here](https://heyer.app/miscapi/statusevent/)

To see how i've applied these status updates, check out "current status" on my landing page [here](https://heyer.app)


## Further Notes


- For any other comments or questions, feel free to reach me on discord via NicholasHeyer#4212




## Authors

- [@nickheyer](https://www.github.com/nickheyer)


## Contributing

Contributions are always welcome!

Email `nick@heyer.app` for ways to get started.
