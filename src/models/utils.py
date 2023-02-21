from pathlib import Path

from playhouse.shortcuts import (
    model_to_dict,
    dict_to_model
)

from models.models import (
    Configuration,
    State,
    ErrLog,
    EventLog,
    File
)

def get_config():
    return Configuration.get()

def get_config_dict():
    return model_to_dict(Configuration.get())

def update_config(data):
    config = Configuration.get()
    if data:
        for k, v in data.items():
            if hasattr(config, k):
                setattr(config, k, v)
    config.save()
    return model_to_dict(config)

def update_state(data):
    state = State.get()
    if data:
        for k, v in data.items():
            if hasattr(state, k):
                setattr(state, k, v)
    state.save()
    return model_to_dict(state)

def get_logs(num):
    return [f"{row.created}: {row.entry}" for row in EventLog.select()
    .order_by(EventLog.created.desc())
    .limit(num)][::-1]

def add_log(entry):
    EventLog.create(entry = entry)

def get_dict(model_str):
    Model = globals()[model_str]
    if Model:
        return model_to_dict(Model.get())
    else:
        return {}

def get_verbose_dict(model_str):
    Model = globals()[model_str]
    if Model:
        fields = model_to_dict(Model.get())
        meta_fields = Model._meta.sorted_fields

        verbose_names = dict()
        field_types = dict()
        
        for field in meta_fields:
            verbose_names[field.name] = field.verbose_name
            field_types[field.name] = field.field_type

        return {
            'fields': fields,
            'verbose': verbose_names,
            'types': field_types
        }
    else:
        return {}
