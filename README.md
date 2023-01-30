![image](https://user-images.githubusercontent.com/60236014/215372009-d6ca97db-f187-4c39-a8d9-d7ac31e5d52a.png)

# DiscoClip
A web-app/bot that monitors discord channels for IG/TikTok video-links then downloads, transcodes (to Discord's size limit), and posts the video to the same channel.

<hr />

## General Requirements
Running the discord bot requires a discord developer account (https://discord.com/developers/applications), and a bot created/invited (via your developer acount) to your chosen discord server.

## Installation

### Linux -- Fully Automated Install & Updates

```bash 
curl https://raw.githubusercontent.com/nickheyer/DiscoClip/main/installer/auto_install_update.sh -o auto_install_update.sh && sudo bash auto_install_update.sh
```

### Other Operating Systems (Windows/Mac) or (Manual Docker Installation)


##### Download Docker Image (x86_64 Architecture) 
```
docker image pull nickheyer/discoclip:latest
```
##### Download Docker Image (aarch64 Architecture, ie: Raspberry-Pi) 
```
docker image pull nickheyer/discoclip_rpi:latest
```
##### Run Docker Container
```
docker run -d -p 7600:7600 nickheyer/discoclip
```
##### The server within the docker container can be accessed locally at [http://127.0.0.1:7600](http://127.0.0.1:7600)
<hr />

## General Instructions

#### Accessing The Web-UI

If you are running DiscoClip on the machine you would like to access it from, you should be able to access it [here](http://127.0.0.1:7600). Otherwise, you will need to get the IP address of the computer hosting DiscoClip. On Windows, you would type `ipconfig` on the host machine and look for your `ipv4`.

If you would like to access DiscoClip remotely, as in not on the same network as the host machine, you will need to do some port forwarding to expose port 7600 to the internet. If you run into any trouble here, feel free to join the [Discord](https://discord.com/invite/6Z9yKTbsrP)!

#### Configuration


![image](https://user-images.githubusercontent.com/60236014/215380220-496a98b3-9262-41b0-86d4-60af6ec096ea.png)
#### *DiscoClip is mostly good to go in terms of configuration. The only thing you will need to provide it is a token from [Discord](https://discord.com/developers/applications). The bot will not start without a valid token. Once your bot is made, you can get the token by clicking 'Reset Token' which will provide it one time only.*

<hr />

![image](https://user-images.githubusercontent.com/60236014/215379968-f63d6682-b1c4-44fd-9107-e4247fc72388.png)
#### *If you haven't already, now is also a good time to invite the bot to the server or servers you would like to monitor, you can do that via the same link.*

<hr />

![image](https://user-images.githubusercontent.com/60236014/215380518-a18661de-24e9-4f3f-81f4-4090214ab386.png)
![image](https://user-images.githubusercontent.com/60236014/215380615-c39618e9-75a6-416c-9770-df80e23082a8.png)
#### *Make sure you select these intents and permissions on your bot page and while generating your invite link (via url generator). It doesn't need to have administrator permissions, that is up to you. Just make sure that it can read and write messages, etc.*

<hr />

## Usage

### Test That The Bot Is Running
Type the following into a discord chat message that the bot can see:

```
!dc test
```

### Your First Clip
Test that the bot is archiving, transcoding, and storing video files properly by pasting a TikTok or Instagram Video url into chat. You can give it a shot with one of my TikTok videos!

Normally, you would not be able to save and upload a video of this size to Discord, but now you can with DiscoClip's transcoding magic! Just paste this link into chat:

```
https://www.tiktok.com/t/ZTRsaDRqY/
```


## Further Notes


- For any other comments or questions, feel free to reach me on discord via NicholasHeyer#4212
- Feel free to join the [Discord](https://discord.com/invite/6Z9yKTbsrP)!




## Authors

- [@nickheyer](https://www.github.com/nickheyer)


## Contributing

Contributions are always welcome!

Email `nick@heyer.app` for ways to get started.
